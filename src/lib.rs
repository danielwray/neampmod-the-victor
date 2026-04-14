use nih_plug::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, atomic};

#[cfg(feature = "gui")]
mod gui;

// Import DSP from neampmod-engine
use neampmod_engine::{
    // Tube modeling
    TubeStage,
    TubeRegistry,
    // AmpTopology
    AmpTopology,
    AmpTopologyConfig,
    ImpedanceConfig,
    // Filters
    DCBlocker,
    // Power supply topology
    FilterChainSpec,
    FilterChainNodeSpec,
    FilterCapSpec,
    FilterResistorSpec,
    // Speaker impedance (SpeakerPreset removed — using physics-based SpeakerModel path)
    // Calibration
    InputCalibration,
    OutputCalibration,
    // Input level metering
    InputLevelMeter,
    // Coupling capacitors
    CouplingCapacitor,
    // Speaker normalizer
    SpeakerNormalizer,
    SpeakerModel,
    // IR loader and convolver
    ir_loader,
    ir_convolver,
    // Pot taper modeling
    PotTaper,
    PotTaperConfig,
    // Input jack modeling
    JackInput,
};

// Embedded IR from assets/ir/default.wav (compiled into binary)
const CABINET_IR_BYTES: &[u8] = include_bytes!("../assets/ir/default.wav");

/// Maps internal 0.0–1.0 parameter value to faceplate numbering (1–12).
fn v2s_dial_1_to_12() -> Arc<dyn Fn(f32) -> String + Send + Sync> {
    Arc::new(move |value: f32| {
        let dial = 1.0 + value * 11.0;
        if dial < 10.0 {
            format!("{:.1}", dial)
        } else {
            format!("{:.0}", dial.round())
        }
    })
}

/// Parses faceplate numbering (1–12) back to internal 0.0–1.0.
fn s2v_dial_1_to_12() -> Arc<dyn Fn(&str) -> Option<f32> + Send + Sync> {
    Arc::new(|string: &str| {
        let dial: f32 = string.trim().parse().ok()?;
        Some(((dial - 1.0) / 11.0).clamp(0.0, 1.0))
    })
}

/// Input jack selection — Hi (full signal) or Lo (~6dB pad)
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputJack {
    #[id = "hi"]
    #[name = "Hi"]
    Hi,
    #[id = "lo"]
    #[name = "Lo"]
    Lo,
}

#[derive(Params)]
struct TheVictorParams {
    /// Volume control — 1MΩ Audio 30A pot, AFTER V1 preamp, before 6V6 grid
    #[id = "volume"]
    pub volume: FloatParam,

    /// Input jack selector — Hi / Lo
    /// Hi: 75kΩ series, full signal
    /// Lo: 75kΩ series + 75kΩ shunt, ~6dB pad
    #[id = "input_jack"]
    pub input_jack: EnumParam<InputJack>,

    /// Master power switch
    #[id = "power"]
    pub power: BoolParam,

    /// Linear master volume control — post-IR, no impact on gain or tone
    #[id = "master"]
    pub master: FloatParam,

    /// Input calibration trim — adjusts input sensitivity
    #[id = "input_trim"]
    pub input_trim_db: FloatParam,

    /// Output calibration trim — adjusts final output level
    #[id = "output_trim"]
    pub output_trim_db: FloatParam,

    /// IR file path — persisted with DAW session state
    #[persist = "ir_path"]
    pub ir_file_path: Arc<Mutex<String>>,
}

impl Default for TheVictorParams {
    fn default() -> Self {
        Self {
            volume: FloatParam::new(
                "Volume",
                0.3,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            input_jack: EnumParam::new("Input", InputJack::Hi),

            power: BoolParam::new("Power", true),

            master: FloatParam::new(
                "Master",
                0.3,
                FloatRange::Linear { min: 0.0001, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            input_trim_db: FloatParam::new(
                "Input Trim",
                0.0,
                FloatRange::Linear { min: -18.0, max: 12.0 },
            )
            .with_unit(" dB")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(5.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            output_trim_db: FloatParam::new(
                "Output Trim",
                0.0,
                FloatRange::Linear { min: -24.0, max: -3.0 },
            )
            .with_unit(" dB")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(5.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            ir_file_path: Arc::new(Mutex::new("default.wav".to_string())),
        }
    }
}

// =============================================================================
// 5C1 Circuit Constants
// =============================================================================

// --- Power Supply ---
/// 5C1 preamp B+ voltage (B+2 tap, after 25kΩ dropping resistor)
/// With 25kΩ drop from ~330V B+1, screen/preamp node measures ~220V under idle current.
const PREAMP_BPLUS_5C1: f32 = 220.0;

// --- Tubes ---
/// V1 — General Electric 6SJ7 sharp-cutoff pentode (preamp)
const V1_STOCK_SPEC: &str = "ge_6sj7_pentode_100k";
/// Power tube — General Electric 6V6GT configured for Champ
const POWER_TUBE_SPEC: &str = "ge_6v6gt_champ_5f1";
/// Rectifier — 5Y3GT
const RECTIFIER_SPEC: &str = "5y3";

// --- 5C1 Volume Pot (between V1 output and 6V6 grid) ---
/// Volume pot total resistance (Ω)
const VOL_POT_R: f32 = 1_000_000.0;
/// V1 output impedance driving the volume pot (Ω).
/// For a pentode with rp >> R_plate, the source impedance ≈ plate load resistor.
const V1_PLATE_LOAD_R: f32 = 250_000.0;

/// Build a preamp TubeStage from the registry with 5C1 circuit values.
/// V1 (6SJ7) uses contact bias: cathode directly grounded, no cathode resistor.
fn build_preamp_tube(sample_rate: f32, spec_name: &str) -> TubeStage {
    let reg = TubeRegistry::global();
    let spec = reg.lookup(spec_name)
        .unwrap_or_else(|| panic!("Tube spec '{}' not found in registry", spec_name));
    // Contact bias: cathode_resistor = 0Ω, no bypass cap
    let mut stage = TubeStage::from_spec(sample_rate, spec, 0.0, None)
        .unwrap_or_else(|e| panic!("Failed to build tube from spec '{}': {}", spec_name, e));
    stage.set_plate_bplus_voltage(PREAMP_BPLUS_5C1);
    // 5MΩ grid leak + 0.02µF coupling cap
    stage.set_grid_leak(5_000_000.0, 0.02e-6, sample_rate);
    stage
}

/// Build the 5C1 power supply filter chain.
///
/// 5C1 filter topology (from schematic):
///   5Y3 → [8µF/450V] → 25kΩ → [8µF/450V]
///
/// B+1 (fc_8u_1): ~330V — power tube plate via OT center tap
/// B+2 (fc_8u_2): ~220V — screen grid + preamp supply
fn build_5c1_filter_chain() -> FilterChainSpec {
    FilterChainSpec {
        nodes: vec![
            // First filter cap after rectifier: B+1 — power tube
            FilterChainNodeSpec::Capacitor(FilterCapSpec {
                instance_id: "fc_8u_1".to_string(),
                capacitance_uf: 8.0,
                voltage_rating: 450.0,
            }),
            // 25kΩ dropping resistor (screen grid + preamp supply)
            FilterChainNodeSpec::Resistor(FilterResistorSpec {
                resistance_ohms: 25_000.0,
            }),
            // Second filter cap: B+2 — preamp supply
            FilterChainNodeSpec::Capacitor(FilterCapSpec {
                instance_id: "fc_8u_2".to_string(),
                capacitance_uf: 8.0,
                voltage_rating: 450.0,
            }),
        ],
        b_plus_assignments: HashMap::from([
            ("power_tube".to_string(), "fc_8u_1".to_string()),
            ("preamp".to_string(), "fc_8u_2".to_string()),
        ]),
        nominal_b_plus_volts: 330.0,
    }
}

/// Build the 5C1 AmpTopology configuration.
///
/// Based on the fender_5f1() preset (closest match: SE 6V6, cathodyne PI,
/// no NFB, 5Y3 rectifier) with 5C1-specific modifications:
/// - Cathode bias: 470Ω (same as 5F1)
/// - Grid leak: 1MΩ (volume pot, not 5F1's fixed 220kΩ resistor)
/// - 2-tap filter chain for B+ distribution (25kΩ dropping resistor)
/// - Jensen P8R speaker impedance (physics-based, 8" alnico, open-back)
/// - Tube specs from registry
fn build_5c1_amp_topology_config() -> AmpTopologyConfig {
    let mut config = AmpTopologyConfig::fender_5f1();

    // Enable current-based sag tracking for authentic 5Y3 rectifier response
    config.power_supply.sag = config.power_supply.sag.with_current_tracking(80.0);

    // Set specific tube specs from registry
    config.power_section.power_tube_spec = Some(POWER_TUBE_SPEC.into());
    config.power_supply.sag.rectifier_spec = Some(RECTIFIER_SPEC.into());

    // 5C1 cathode bias: 470Ω Rk, 25µF Ck, ~5kΩ plate load (OT primary)
    config.power_section.cathode_bias = Some((470.0, 25e-6, 5_000.0));

    // 5C1 has no fixed grid-to-ground resistor on the 6V6 — the volume pot
    // bottom rail (wiper to ground) serves as the grid return path.
    // Override 5F1's 220kΩ grid leak with 1MΩ (full pot resistance) to
    // approximate the highest-impedance condition (full volume).
    // The actual grid-to-ground R varies with wiper position (R_bot), but
    // the engine's PI coupling uses a static value.
    config.power_section.pi_grid_leak_ohms = 1_000_000.0;

    // 5C1 power supply filter chain (2-tap: power_tube + preamp)
    config.filter_chain = Some(build_5c1_filter_chain());

    // 8" Jensen P8R speaker impedance (physics-based), open-back cabinet
    config.impedance = Some(ImpedanceConfig {
        speaker_model: Some(SpeakerModel::JensenP8R),
        cabinet_factor_override: Some(0.75), // open-back
        ..Default::default()
    });

    config
}

/// Compute the meter ceiling at the amp jack for V1 tube stage.
/// ceiling = clean_ac_ceiling_volts / jack.dc_gain()
fn meter_ceiling_for_tube(tube: &TubeStage, jack: &JackInput) -> f32 {
    tube.voltage_cal().clean_ac_ceiling_volts() / jack.dc_gain()
}

// =============================================================================
// TheVictor — Fender Champ 5C1 Plugin
// =============================================================================

pub struct TheVictor {
    params: Arc<TheVictorParams>,
    sample_rate: f32,

    // === Input ===
    input_cal: InputCalibration,
    jack_hi: JackInput,  // Hi: 75kΩ series, 5MΩ grid leak to ground
    jack_lo: JackInput,  // Lo: 75kΩ series, 75kΩ shunt to ground (~6dB pad)
    output_cal: OutputCalibration,

    // === Volume pot (1MΩ Audio 30A, AFTER V1, before 6V6) ===
    volume_taper: PotTaperConfig,

    // === Coupling capacitor ===
    // Note: coupling_in (jack → V1 grid) is handled by the GridCurrentModel
    // inside v1_tube, which models both DC blocking AND nonlinear grid conduction.
    // V1 plate → volume pot top: 0.02µF, 1MΩ pot load (fc ≈ 8Hz, pure DC blocker)
    coupling_out: CouplingCapacitor,

    // === V1 preamp (6SJ7 pentode, contact bias) ===
    v1_tube: TubeStage,

    // === AmpTopology: PI → 6V6GT SE → OT → speaker impedance + PSU ===
    amp_topology: AmpTopology,

    // === Speaker normalizer (OT secondary volts → normalized ±1 for IR) ===
    speaker_normalizer: SpeakerNormalizer,

    // === IR convolution (block-based, matched to DAW buffer size) ===
    ir_convolver: ir_convolver::ZeroLatencyConvolver,
    pre_ir_buffer: Vec<f32>,
    post_ir_buffer: Vec<f32>,
    ir_block_size: usize,

    // === Output ===
    dc_blocker_output: DCBlocker,

    // === Metering ===
    input_meter: InputLevelMeter,
    cached_input_trim_db: f32,

    // Shared with GUI (written once per buffer from audio thread)
    meter_peak_volts: Arc<atomic_float::AtomicF32>,

    // IR loading state (shared with GUI)
    ir_load_status: Arc<atomic::AtomicU8>,  // 0=pending, 1=success, 2=failed
}

impl Default for TheVictor {
    fn default() -> Self {
        let sample_rate = 48000.0;

        // Build input chain components
        let input_cal = InputCalibration::amp_standard();

        // 5C1 Hi jack: 75kΩ series, 5MΩ grid leak to ground
        let jack_hi = JackInput::new(75_000.0, 5_000_000.0);
        // 5C1 Lo jack: 75kΩ series, 75kΩ shunt to ground (~6dB pad)
        let jack_lo = JackInput::new(75_000.0, 75_000.0);

        // Build V1 tube before struct so meter can read ceiling
        let v1_tube = build_preamp_tube(sample_rate, V1_STOCK_SPEC);

        // Input meter — use Hi jack for default ceiling calculation
        let meter_ceiling = meter_ceiling_for_tube(&v1_tube, &jack_hi);
        let input_meter = InputLevelMeter::new(sample_rate, input_cal.input_scale(), meter_ceiling);

        Self {
            params: Arc::new(TheVictorParams::default()),
            sample_rate,

            input_cal,
            jack_hi,
            jack_lo,
            output_cal: OutputCalibration::pro_audio_headroom(),

            // 1MΩ Audio 30A pot (5C1 volume control)
            volume_taper: PotTaperConfig::new(PotTaper::Audio30A),

            // Coupling cap: V1 plate → volume pot (0.02µF, 1MΩ pot resistance)
            coupling_out: CouplingCapacitor::new(sample_rate, 0.02e-6, 1_000_000.0),

            v1_tube,

            // AmpTopology: PI → 6V6GT SE → OT → speaker impedance + PSU
            amp_topology: AmpTopology::new(sample_rate, build_5c1_amp_topology_config()),

            speaker_normalizer: SpeakerNormalizer::from_speaker_model(SpeakerModel::JensenP8R),

            // IR convolution — load embedded default.wav
            ir_convolver: {
                let ir_loader = ir_loader::IrLoader::new(sample_rate);
                match ir_loader.load_from_bytes(CABINET_IR_BYTES) {
                    Ok((ir, _, _)) => {
                        let mut processed_ir = ir;
                        ir_loader::IrLoader::remove_dc_offset(&mut processed_ir);
                        ir_loader::IrLoader::normalize_rms(&mut processed_ir, -12.0);
                        ir_convolver::ZeroLatencyConvolver::new(&processed_ir, 512, 128)
                    }
                    Err(_) => {
                        // Fallback: unity impulse (bypass)
                        ir_convolver::ZeroLatencyConvolver::new(&[1.0], 512, 1)
                    }
                }
            },
            pre_ir_buffer: vec![0.0; 512],
            post_ir_buffer: vec![0.0; 512],
            ir_block_size: 512,

            dc_blocker_output: DCBlocker::new(sample_rate, 10.0),

            input_meter,
            cached_input_trim_db: 0.0,

            meter_peak_volts: Arc::new(atomic_float::AtomicF32::new(0.0)),

            ir_load_status: Arc::new(atomic::AtomicU8::new(1)), // Start with success (embedded IR)
        }
    }
}

impl TheVictor {
    /// Load IR from file path. Returns true if successful.
    pub fn load_ir_from_file(&mut self, path: &std::path::Path) -> bool {
        use neampmod_engine::{ir_loader::IrLoader, ir_convolver::ZeroLatencyConvolver};

        let ir_loader = IrLoader::new(self.sample_rate);

        match ir_loader.load_from_file(path) {
            Ok((mut ir, _, _)) => {
                IrLoader::remove_dc_offset(&mut ir);
                IrLoader::normalize_rms(&mut ir, -12.0);

                let fir_len = 128.min(self.ir_block_size);
                self.ir_convolver = ZeroLatencyConvolver::new(&ir, self.ir_block_size, fir_len);

                self.ir_load_status.store(1, atomic::Ordering::Relaxed);
                if let Ok(mut path_str) = self.params.ir_file_path.lock() {
                    *path_str = path.display().to_string();
                }

                true
            }
            Err(_) => {
                self.ir_load_status.store(2, atomic::Ordering::Relaxed);
                false
            }
        }
    }
}

impl Plugin for TheVictor {
    const NAME: &'static str = "The Victor";
    const VENDOR: &'static str = "neampmod";
    const URL: &'static str = env!("CARGO_PKG_HOMEPAGE");
    const EMAIL: &'static str = env!("CARGO_PKG_AUTHORS");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(1),
        main_output_channels: NonZeroU32::new(1),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;
        self.ir_block_size = buffer_config.max_buffer_size as usize;

        // Resize IR processing buffers to match DAW buffer size
        self.pre_ir_buffer.resize(self.ir_block_size, 0.0);
        self.post_ir_buffer.resize(self.ir_block_size, 0.0);

        // Initialize parameter smoothing
        self.params.volume.smoothed.reset(self.params.volume.value());
        self.params.master.smoothed.reset(self.params.master.value());

        // Rebuild V1 tube at new sample rate
        self.v1_tube = build_preamp_tube(self.sample_rate, V1_STOCK_SPEC);

        // Rebuild coupling_out at new sample rate (coupling_in is inside v1_tube's grid model)
        self.coupling_out = CouplingCapacitor::new(self.sample_rate, 0.02e-6, 1_000_000.0);

        // Reinitialize AmpTopology (PI → power tube → OT → impedance + power supply)
        self.amp_topology = AmpTopology::new(self.sample_rate, build_5c1_amp_topology_config());
        self.speaker_normalizer = SpeakerNormalizer::from_speaker_model(SpeakerModel::JensenP8R);

        // Reload IR convolver with new sample rate and DAW buffer size
        let persisted_ir_path = self.params.ir_file_path.lock()
            .map(|p| p.clone())
            .unwrap_or_else(|_| "default.wav".to_string());

        let ir_reloaded = if persisted_ir_path != "default.wav" {
            let path = std::path::PathBuf::from(&persisted_ir_path);
            if path.exists() {
                self.load_ir_from_file(&path)
            } else {
                false
            }
        } else {
            false
        };

        if !ir_reloaded {
            let ir_loader = ir_loader::IrLoader::new(self.sample_rate);
            if let Ok((ir, _, _)) = ir_loader.load_from_bytes(CABINET_IR_BYTES) {
                let mut processed_ir = ir;
                ir_loader::IrLoader::remove_dc_offset(&mut processed_ir);
                ir_loader::IrLoader::normalize_rms(&mut processed_ir, -12.0);
                let fir_len = 128.min(self.ir_block_size);
                self.ir_convolver = ir_convolver::ZeroLatencyConvolver::new(&processed_ir, self.ir_block_size, fir_len);
            }
            if persisted_ir_path != "default.wav" {
                if let Ok(mut p) = self.params.ir_file_path.lock() {
                    *p = "default.wav".to_string();
                }
                self.ir_load_status.store(2, atomic::Ordering::Relaxed);
            }
        }

        self.dc_blocker_output = DCBlocker::new(self.sample_rate, 10.0);

        // Sync input trim into InputCalibration and rebuild meter
        let trim_db = self.params.input_trim_db.value();
        self.input_cal.set_user_trim_db(trim_db);
        self.cached_input_trim_db = trim_db;
        let ceiling = meter_ceiling_for_tube(&self.v1_tube, &self.jack_hi);
        self.input_meter = InputLevelMeter::new(
            self.sample_rate,
            self.input_cal.input_scale(),
            ceiling,
        );

        true
    }

    fn reset(&mut self) {
        // Reset parameter smoothing
        self.params.volume.smoothed.reset(self.params.volume.value());
        self.params.master.smoothed.reset(self.params.master.value());

        // Reset input jacks
        self.jack_hi.reset();
        self.jack_lo.reset();

        // Reset V1 preamp tube
        self.v1_tube.reset();

        // Reset coupling capacitor (coupling_in is reset via v1_tube.reset())
        self.coupling_out.reset();

        // Reset AmpTopology (PI, power tube, OT, power supply, speaker impedance)
        self.amp_topology.reset();

        // Reset IR convolver and output
        self.ir_convolver.reset();
        self.pre_ir_buffer.fill(0.0);
        self.post_ir_buffer.fill(0.0);
        self.dc_blocker_output.reset();

        // Reset input meter
        self.input_meter.reset();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Check for pending IR load (once per buffer)
        if self.ir_load_status.load(atomic::Ordering::Relaxed) == 0 {
            let path_opt = self.params.ir_file_path.try_lock()
                .ok()
                .map(|guard| std::path::PathBuf::from(guard.as_str()));

            if let Some(path) = path_opt {
                self.load_ir_from_file(&path);
            }
        }

        let num_samples = buffer.samples();
        let power_on = self.params.power.value();
        let mut sample_idx = 0usize;

        // === INPUT TRIM → InputCalibration (once per buffer, change-detected) ===
        let current_trim_db = self.params.input_trim_db.value();
        if (current_trim_db - self.cached_input_trim_db).abs() > 0.01 {
            self.cached_input_trim_db = current_trim_db;
            self.input_cal.set_user_trim_db(current_trim_db);
            self.input_meter.set_input_scale(self.input_cal.input_scale());
        }

        // === AmpTopology: begin buffer ===
        self.amp_topology.begin_buffer(num_samples);

        // === PASS 1: Per-sample signal chain ===
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                if !power_on {
                    self.pre_ir_buffer[sample_idx] = 0.0;
                    sample_idx += 1;
                    continue;
                }

                let input = *sample;

                // Advance power supply interpolation
                self.amp_topology.advance_sample();

                // === Get smoothed volume parameter ===
                let vol_raw = self.params.volume.smoothed.next();
                let wiper = self.volume_taper.wiper_fraction(vol_raw);
                let input_jack = self.params.input_jack.value();

                // === INPUT LEVEL METER (raw DAW signal, before calibration) ===
                self.input_meter.process(input);

                // === INPUT CALIBRATION ===
                let mut signal = self.input_cal.process(input);

                // === INPUT JACK VOLTAGE DIVIDER ===
                signal = match input_jack {
                    InputJack::Hi => self.jack_hi.process(signal),
                    InputJack::Lo => self.jack_lo.process(signal),
                };

                // === V1 GRID COUPLING (handled internally by TubeStage) ===
                // The GridCurrentModel inside v1_tube models the 0.02µF coupling
                // cap + 5MΩ grid leak as a nonlinear element: DC blocking plus
                // grid conduction rectification → blocking distortion (τ = 100ms).

                // === B+ for preamp (from AmpTopology power supply) ===
                let b_plus_preamp = self.amp_topology.b_plus_for_stage("preamp");

                // === V1 PREAMP (6SJ7 pentode, contact bias) ===
                // bias = 0.0 because contact bias means cathode is grounded,
                // no external bias voltage from a cathode RC circuit.
                // V1 runs at full gain regardless of volume setting — the 5C1
                // design places the volume pot AFTER the preamp.
                signal = self.v1_tube.process(signal, 0.0, b_plus_preamp);

                // === COUPLING CAP OUT (0.02µF, V1 plate → volume pot, 1MΩ pot load) ===
                signal = self.coupling_out.process(signal);

                // === VOLUME POT (1MΩ Audio 30A, AFTER V1) ===
                // Models voltage divider with V1 output impedance as source:
                //   V_out = V_in × R_bot / (R_source + R_top + R_bot)
                // where R_top = (1-wiper) × R_pot, R_bot = wiper × R_pot,
                // R_source ≈ 250kΩ (V1 plate load; pentode rp >> R_plate).
                {
                    let r_bot = wiper * VOL_POT_R;
                    let r_top = (1.0 - wiper) * VOL_POT_R;
                    let divider = r_bot / (V1_PLATE_LOAD_R + r_top + r_bot);
                    signal *= divider;
                }

                // === POWER SECTION (PI → 6V6GT SE → OT → speaker impedance) ===
                let ot_volts = self.amp_topology.process_power_section(signal);

                // === NORMALIZE SPEAKER (physical OT secondary volts → ±1 for IR) ===
                signal = self.speaker_normalizer.process(ot_volts);

                // Store pre-IR signal for block convolution
                self.pre_ir_buffer[sample_idx] = signal;
                sample_idx += 1;
            }
        }

        // === AmpTopology: end buffer ===
        self.amp_topology.end_buffer(&[]);

        // === PASS 2: Block IR convolution (zero-latency, matched to DAW buffer) ===
        for i in num_samples..self.ir_block_size {
            self.pre_ir_buffer[i] = 0.0;
        }
        self.ir_convolver.process(
            &self.pre_ir_buffer[..self.ir_block_size],
            &mut self.post_ir_buffer[..self.ir_block_size],
        );

        // === PASS 3: Post-IR processing (output cal, master, DC block) ===
        {
            let output_channel = &mut buffer.as_slice()[0];
            for i in 0..num_samples {
                if !power_on {
                    output_channel[i] = 0.0;
                    continue;
                }

                let mut signal = self.post_ir_buffer[i];

                // Output calibration
                signal = self.output_cal.process(signal);
                let output_trim = self.params.output_trim_db.smoothed.next();
                signal *= neampmod_engine::db_to_linear(output_trim);

                // Master volume (linear, post-IR, no tone impact)
                let master = self.params.master.smoothed.next();
                let master_gain = master.powf(1.5);
                signal *= master_gain;

                // DC blocking
                signal = self.dc_blocker_output.process(signal);

                output_channel[i] = signal;
            }
        }

        // === METER: snapshot metrics for GUI (once per buffer) ===
        let metrics = self.input_meter.get_metrics();
        self.meter_peak_volts.store(metrics.peak_volts, atomic::Ordering::Relaxed);

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        #[cfg(feature = "gui")]
        {
            use nih_plug_egui::{create_egui_editor, EguiState};

            let params = self.params.clone();
            let ir_status = self.ir_load_status.clone();
            let ir_path = self.params.ir_file_path.clone();
            let meter_peak_volts = self.meter_peak_volts.clone();

            create_egui_editor(
                EguiState::from_size(800, 450),
                gui::GuiState::new(ir_status, ir_path, meter_peak_volts),
                |_, _| {},
                move |egui_ctx, setter, state| {
                    gui::create(egui_ctx, setter, &params, state)
                },
            )
        }
        #[cfg(not(feature = "gui"))]
        {
            None
        }
    }
}

impl ClapPlugin for TheVictor {
    const CLAP_ID: &'static str = "com.neampmod.the-victor";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Circuit-accurate model of the Fender Champ 5C1 guitar amplifier.");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Distortion,
        ClapFeature::Stereo,
        ClapFeature::Mono,
    ];
}

impl Vst3Plugin for TheVictor {
    const VST3_CLASS_ID: [u8; 16] = *b"TheVictor.......";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Distortion];
}

// Export as CLAP plugin
nih_export_clap!(TheVictor);

// Export as VST3 plugin
nih_export_vst3!(TheVictor);
