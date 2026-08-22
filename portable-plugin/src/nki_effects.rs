//! Real-time-safe DSP for the audible subset of Kontakt insert racks.

use rackforge_dsp::{Reverb, ReverbParameters, StereoFrame};
use rf_soundfonts::nki::document::{NkiDelay, NkiEffects, NkiFilter, NkiProgramEffect};
use std::f32::consts::{LN_2, TAU};

const MAX_DELAY_SECONDS: f32 = 2.0;
const PARAMETER_SMOOTHING_SECONDS: f32 = 0.030;

pub const REVERB_SIZE_INDEX: u32 = 2;
pub const REVERB_DECAY_INDEX: u32 = 3;
pub const REVERB_PRE_DELAY_INDEX: u32 = 4;
pub const REVERB_DAMPING_INDEX: u32 = 5;
pub const REVERB_WIDTH_INDEX: u32 = 6;
pub const DELAY_TIME_INDEX: u32 = 7;
pub const DELAY_FEEDBACK_INDEX: u32 = 8;
pub const DELAY_DAMPING_INDEX: u32 = 9;
pub const DELAY_STEREO_INDEX: u32 = 10;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectControls {
    pub reverb_size: f32,
    pub reverb_decay: f32,
    pub reverb_pre_delay: f32,
    pub reverb_damping: f32,
    pub reverb_width: f32,
    pub delay_time: f32,
    pub delay_feedback: f32,
    pub delay_damping: f32,
    pub delay_stereo: f32,
}

impl Default for EffectControls {
    fn default() -> Self {
        Self {
            reverb_size: 1.0,
            reverb_decay: 1.0,
            reverb_pre_delay: 1.0,
            reverb_damping: 0.0,
            reverb_width: 1.0,
            delay_time: 1.0,
            delay_feedback: 0.0,
            delay_damping: 0.0,
            delay_stereo: 1.0,
        }
    }
}

impl EffectControls {
    pub fn set_parameter(&mut self, index: u32, value: f32) -> bool {
        if !value.is_finite() {
            return false;
        }
        match index {
            REVERB_SIZE_INDEX => self.reverb_size = value.clamp(0.5, 1.5),
            REVERB_DECAY_INDEX => self.reverb_decay = value.clamp(0.25, 4.0),
            REVERB_PRE_DELAY_INDEX => self.reverb_pre_delay = value.clamp(0.0, 2.0),
            REVERB_DAMPING_INDEX => self.reverb_damping = value.clamp(-0.5, 0.5),
            REVERB_WIDTH_INDEX => self.reverb_width = value.clamp(0.0, 1.5),
            DELAY_TIME_INDEX => self.delay_time = value.clamp(0.25, 4.0),
            DELAY_FEEDBACK_INDEX => self.delay_feedback = value.clamp(-0.5, 0.2),
            DELAY_DAMPING_INDEX => self.delay_damping = value.clamp(-0.5, 0.5),
            DELAY_STEREO_INDEX => self.delay_stereo = value.clamp(0.0, 1.5),
            _ => return false,
        }
        true
    }

    pub fn parameter(self, index: u32) -> Option<f64> {
        let value = match index {
            REVERB_SIZE_INDEX => self.reverb_size,
            REVERB_DECAY_INDEX => self.reverb_decay,
            REVERB_PRE_DELAY_INDEX => self.reverb_pre_delay,
            REVERB_DAMPING_INDEX => self.reverb_damping,
            REVERB_WIDTH_INDEX => self.reverb_width,
            DELAY_TIME_INDEX => self.delay_time,
            DELAY_FEEDBACK_INDEX => self.delay_feedback,
            DELAY_DAMPING_INDEX => self.delay_damping,
            DELAY_STEREO_INDEX => self.delay_stereo,
            _ => return None,
        };
        Some(f64::from(value))
    }
}

pub struct EffectRack {
    nodes: Vec<EffectNode>,
}

enum EffectNode {
    Reverb {
        processor: Box<Reverb>,
        base: ReverbParameters,
        dry_gain: f32,
        wet_gain: f32,
    },
    Delay(StereoDelay),
}

impl EffectRack {
    pub fn new(specification: &NkiEffects, sample_rate: u32) -> Result<Self, &'static str> {
        let mut nodes = Vec::with_capacity(specification.program.len());
        for effect in &specification.program {
            match *effect {
                NkiProgramEffect::Reverb(specification) => {
                    let mut processor =
                        Reverb::new(sample_rate as f32).map_err(|_| "invalid reverb rate")?;
                    let base = ReverbParameters {
                        enabled: true,
                        size: 0.5 + specification.room_size,
                        decay_seconds: 0.35 + specification.room_size * 5.65,
                        pre_delay_ms: specification.pre_delay_ms,
                        // Kontakt's filter and color controls both brighten as
                        // they rise; RF's damping control closes as it rises.
                        damping: 1.0 - (specification.damping * 0.65 + specification.color * 0.35),
                        width: specification.width,
                        mix: 1.0,
                    };
                    processor.set_parameters(base)?;
                    processor.reset();
                    nodes.push(EffectNode::Reverb {
                        processor: Box::new(processor),
                        base,
                        dry_gain: specification.dry_gain,
                        wet_gain: specification.wet_gain,
                    });
                }
                NkiProgramEffect::Delay(specification) => {
                    nodes.push(EffectNode::Delay(StereoDelay::new(
                        specification,
                        sample_rate,
                    )));
                }
            }
        }
        Ok(Self { nodes })
    }

    pub fn set_controls(&mut self, controls: EffectControls) {
        for node in &mut self.nodes {
            match node {
                EffectNode::Reverb {
                    processor, base, ..
                } => {
                    let parameters = ReverbParameters {
                        enabled: true,
                        size: (base.size * controls.reverb_size).clamp(0.5, 1.5),
                        decay_seconds: (base.decay_seconds * controls.reverb_decay)
                            .clamp(0.2, 12.0),
                        pre_delay_ms: (base.pre_delay_ms * controls.reverb_pre_delay)
                            .clamp(0.0, 200.0),
                        damping: (base.damping + controls.reverb_damping).clamp(0.0, 1.0),
                        width: (base.width * controls.reverb_width).clamp(0.0, 1.0),
                        mix: 1.0,
                    };
                    // Every field is clamped to rackforge-dsp's public range.
                    let _ = processor.set_parameters(parameters);
                }
                EffectNode::Delay(processor) => processor.set_controls(controls),
            }
        }
    }

    pub fn reset(&mut self) {
        for node in &mut self.nodes {
            match node {
                EffectNode::Reverb { processor, .. } => processor.reset(),
                EffectNode::Delay(processor) => processor.reset(),
            }
        }
    }

    #[inline]
    pub fn process(&mut self, frame: [f32; 2], amount: f32) -> [f32; 2] {
        let amount = amount.clamp(0.0, 1.0);
        let mut frame = StereoFrame::new(frame[0], frame[1]);
        for node in &mut self.nodes {
            frame = match node {
                EffectNode::Reverb {
                    processor,
                    base: _,
                    dry_gain,
                    wet_gain,
                } => {
                    let wet = processor.process(frame);
                    StereoFrame::new(
                        frame.left * *dry_gain + wet.left * *wet_gain * amount,
                        frame.right * *dry_gain + wet.right * *wet_gain * amount,
                    )
                }
                EffectNode::Delay(processor) => processor.process(frame, amount),
            };
        }
        [finite(frame.left), finite(frame.right)]
    }
}

struct StereoDelay {
    left: DelayLine,
    right: DelayLine,
    base_delay_samples: f32,
    base_feedback: f32,
    base_crossfeed: f32,
    base_damping: f32,
    sample_rate: f32,
    delay_samples: Smoothed,
    feedback: Smoothed,
    crossfeed: Smoothed,
    damping_coefficient: Smoothed,
    damping_state: [f32; 2],
    dry_gain: f32,
    wet_gain: f32,
}

impl StereoDelay {
    fn new(specification: NkiDelay, sample_rate: u32) -> Self {
        let sample_rate = sample_rate as f32;
        let capacity = (sample_rate * MAX_DELAY_SECONDS).ceil() as usize + 4;
        let openness = 1.0 - specification.damping;
        let cutoff_hz = 500.0 + 17_500.0 * openness * openness;
        let delay_samples = (specification.time_ms * 0.001 * sample_rate)
            .clamp(1.0, capacity.saturating_sub(2) as f32);
        let smoothing = smoothing_coefficient(sample_rate);
        Self {
            left: DelayLine::new(capacity),
            right: DelayLine::new(capacity),
            base_delay_samples: delay_samples,
            base_feedback: specification.feedback,
            base_crossfeed: specification.panning.abs().clamp(0.0, 1.0),
            base_damping: specification.damping,
            sample_rate,
            delay_samples: Smoothed::new(delay_samples, smoothing),
            feedback: Smoothed::new(specification.feedback, smoothing),
            crossfeed: Smoothed::new(specification.panning.abs().clamp(0.0, 1.0), smoothing),
            damping_coefficient: Smoothed::new(
                1.0 - (-TAU * cutoff_hz / sample_rate).exp(),
                smoothing,
            ),
            damping_state: [0.0; 2],
            dry_gain: specification.dry_gain,
            wet_gain: specification.wet_gain,
        }
    }

    fn set_controls(&mut self, controls: EffectControls) {
        let maximum_delay = self.left.samples.len().saturating_sub(2) as f32;
        self.delay_samples
            .set_target((self.base_delay_samples * controls.delay_time).clamp(1.0, maximum_delay));
        self.feedback
            .set_target((self.base_feedback + controls.delay_feedback).clamp(0.0, 0.98));
        self.crossfeed
            .set_target((self.base_crossfeed * controls.delay_stereo).clamp(0.0, 1.0));
        let damping = (self.base_damping + controls.delay_damping).clamp(0.0, 1.0);
        let openness = 1.0 - damping;
        let cutoff_hz = 500.0 + 17_500.0 * openness * openness;
        self.damping_coefficient
            .set_target(1.0 - (-TAU * cutoff_hz / self.sample_rate).exp());
    }

    fn reset(&mut self) {
        self.left.clear();
        self.right.clear();
        self.damping_state = [0.0; 2];
    }

    #[inline]
    fn process(&mut self, input: StereoFrame, amount: f32) -> StereoFrame {
        let delay_samples = self.delay_samples.next();
        let feedback = self.feedback.next();
        let crossfeed = self.crossfeed.next();
        let damping_coefficient = self.damping_coefficient.next();
        let delayed = [
            self.left.read(delay_samples),
            self.right.read(delay_samples),
        ];
        for (state, sample) in self.damping_state.iter_mut().zip(delayed) {
            *state += (sample - *state) * damping_coefficient;
            *state = finite(*state);
        }
        let feedback_left =
            self.damping_state[0] * (1.0 - crossfeed) + self.damping_state[1] * crossfeed;
        let feedback_right =
            self.damping_state[1] * (1.0 - crossfeed) + self.damping_state[0] * crossfeed;
        self.left
            .write(finite(input.left + feedback_left * feedback));
        self.right
            .write(finite(input.right + feedback_right * feedback));
        StereoFrame::new(
            input.left * self.dry_gain + delayed[0] * self.wet_gain * amount,
            input.right * self.dry_gain + delayed[1] * self.wet_gain * amount,
        )
    }
}

struct Smoothed {
    current: f32,
    target: f32,
    coefficient: f32,
}

impl Smoothed {
    fn new(value: f32, coefficient: f32) -> Self {
        Self {
            current: value,
            target: value,
            coefficient,
        }
    }

    fn set_target(&mut self, target: f32) {
        if target.is_finite() {
            self.target = target;
        }
    }

    #[inline]
    fn next(&mut self) -> f32 {
        self.current += (self.target - self.current) * self.coefficient;
        self.current = finite(self.current);
        self.current
    }
}

fn smoothing_coefficient(sample_rate: f32) -> f32 {
    1.0 - (-1.0 / (sample_rate * PARAMETER_SMOOTHING_SECONDS)).exp()
}

struct DelayLine {
    samples: Box<[f32]>,
    write_index: usize,
}

impl DelayLine {
    fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0.0; capacity.max(4)].into_boxed_slice(),
            write_index: 0,
        }
    }

    fn clear(&mut self) {
        self.samples.fill(0.0);
        self.write_index = 0;
    }

    #[inline]
    fn read(&self, delay: f32) -> f32 {
        let length = self.samples.len() as f32;
        let position = (self.write_index as f32 - delay).rem_euclid(length);
        let base = position.floor();
        let first = base as usize % self.samples.len();
        let second = (first + 1) % self.samples.len();
        let fraction = position - base;
        self.samples[first] + (self.samples[second] - self.samples[first]) * fraction
    }

    #[inline]
    fn write(&mut self, sample: f32) {
        self.samples[self.write_index] = sample;
        self.write_index = (self.write_index + 1) % self.samples.len();
    }
}

pub struct StereoBiquad {
    coefficients: BiquadCoefficients,
    state: [[f32; 2]; 2],
}

#[derive(Clone, Copy)]
struct BiquadCoefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl StereoBiquad {
    pub fn new(specification: NkiFilter, sample_rate: u32) -> Option<Self> {
        let sample_rate = sample_rate as f32;
        if !(8_000.0..=384_000.0).contains(&sample_rate) {
            return None;
        }
        let coefficients = match specification {
            NkiFilter::LowPass2 { cutoff, resonance } => {
                let cutoff_hz = (cutoff * 20_000.0).clamp(20.0, sample_rate * 0.45);
                let q = 0.707 + resonance * 9.293;
                low_pass(cutoff_hz, q, sample_rate)
            }
            NkiFilter::HighPass2 { cutoff, resonance } => {
                let cutoff_hz = (cutoff * 20_000.0).clamp(20.0, sample_rate * 0.45);
                let q = 0.707 + resonance * 9.293;
                high_pass(cutoff_hz, q, sample_rate)
            }
            NkiFilter::PeakEq {
                frequency_hz,
                bandwidth_octaves,
                gain_db,
            } => {
                if gain_db.abs() < 0.001 {
                    return None;
                }
                peak_eq(
                    frequency_hz.clamp(20.0, sample_rate * 0.45),
                    bandwidth_octaves,
                    gain_db,
                    sample_rate,
                )
            }
        }?;
        Some(Self {
            coefficients,
            state: [[0.0; 2]; 2],
        })
    }

    #[inline]
    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        [
            self.process_channel(0, frame[0]),
            self.process_channel(1, frame[1]),
        ]
    }

    #[inline]
    fn process_channel(&mut self, channel: usize, input: f32) -> f32 {
        let state = &mut self.state[channel];
        let output = self.coefficients.b0 * input + state[0];
        state[0] = finite(self.coefficients.b1 * input - self.coefficients.a1 * output + state[1]);
        state[1] = finite(self.coefficients.b2 * input - self.coefficients.a2 * output);
        finite(output)
    }
}

fn low_pass(frequency: f32, q: f32, sample_rate: f32) -> Option<BiquadCoefficients> {
    let omega = TAU * frequency / sample_rate;
    let cosine = omega.cos();
    let alpha = omega.sin() / (2.0 * q.max(0.05));
    normalise_biquad(
        (1.0 - cosine) * 0.5,
        1.0 - cosine,
        (1.0 - cosine) * 0.5,
        1.0 + alpha,
        -2.0 * cosine,
        1.0 - alpha,
    )
}

fn high_pass(frequency: f32, q: f32, sample_rate: f32) -> Option<BiquadCoefficients> {
    let omega = TAU * frequency / sample_rate;
    let cosine = omega.cos();
    let alpha = omega.sin() / (2.0 * q.max(0.05));
    normalise_biquad(
        (1.0 + cosine) * 0.5,
        -(1.0 + cosine),
        (1.0 + cosine) * 0.5,
        1.0 + alpha,
        -2.0 * cosine,
        1.0 - alpha,
    )
}

fn peak_eq(
    frequency: f32,
    bandwidth_octaves: f32,
    gain_db: f32,
    sample_rate: f32,
) -> Option<BiquadCoefficients> {
    let omega = TAU * frequency / sample_rate;
    let sine = omega.sin();
    let cosine = omega.cos();
    let amplitude = 10.0_f32.powf(gain_db / 40.0);
    let alpha = sine * (LN_2 * 0.5 * bandwidth_octaves * omega / sine.abs().max(1.0e-6)).sinh();
    normalise_biquad(
        1.0 + alpha * amplitude,
        -2.0 * cosine,
        1.0 - alpha * amplitude,
        1.0 + alpha / amplitude,
        -2.0 * cosine,
        1.0 - alpha / amplitude,
    )
}

fn normalise_biquad(
    b0: f32,
    b1: f32,
    b2: f32,
    a0: f32,
    a1: f32,
    a2: f32,
) -> Option<BiquadCoefficients> {
    if !a0.is_finite() || a0.abs() < f32::MIN_POSITIVE {
        return None;
    }
    let value = BiquadCoefficients {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    };
    [value.b0, value.b1, value.b2, value.a1, value.a2]
        .iter()
        .all(|coefficient| coefficient.is_finite())
        .then_some(value)
}

#[inline]
fn finite(sample: f32) -> f32 {
    if sample.is_finite() && sample.abs() >= 1.0e-20 {
        sample
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rf_soundfonts::nki::document::{NkiProgramEffect, NkiReverb};

    fn response_magnitude(coefficients: BiquadCoefficients, frequency: f32, rate: f32) -> f32 {
        let omega = TAU * frequency / rate;
        let (sin_one, cos_one) = omega.sin_cos();
        let (sin_two, cos_two) = (2.0 * omega).sin_cos();
        let numerator_real =
            coefficients.b0 + coefficients.b1 * cos_one + coefficients.b2 * cos_two;
        let numerator_imaginary = -coefficients.b1 * sin_one - coefficients.b2 * sin_two;
        let denominator_real = 1.0 + coefficients.a1 * cos_one + coefficients.a2 * cos_two;
        let denominator_imaginary = -coefficients.a1 * sin_one - coefficients.a2 * sin_two;
        ((numerator_real * numerator_real + numerator_imaginary * numerator_imaginary)
            / (denominator_real * denominator_real + denominator_imaginary * denominator_imaginary))
            .sqrt()
    }

    #[test]
    fn a_reverb_tail_is_finite_and_outlives_the_impulse() {
        let effects = NkiEffects {
            program: vec![NkiProgramEffect::Reverb(NkiReverb {
                pre_delay_ms: 0.0,
                room_size: 0.7,
                width: 0.8,
                color: 0.5,
                damping: 0.5,
                wet_gain: 0.5,
                dry_gain: 1.0,
            })],
            ..NkiEffects::default()
        };
        let mut rack = EffectRack::new(&effects, 48_000).unwrap();
        let mut tail_peak = 0.0_f32;
        for frame in 0..24_000 {
            let input = if frame == 0 { [1.0, 1.0] } else { [0.0, 0.0] };
            let output = rack.process(input, 1.0);
            assert!(output.iter().all(|sample| sample.is_finite()));
            if frame > 4_000 {
                tail_peak = tail_peak.max(output[0].abs()).max(output[1].abs());
            }
        }
        assert!(tail_peak > 1.0e-5);
    }

    #[test]
    fn delay_feedback_decays_without_running_away() {
        let mut delay = StereoDelay::new(
            NkiDelay {
                time_ms: 10.0,
                feedback: 0.75,
                panning: 0.5,
                damping: 0.2,
                wet_gain: 1.0,
                dry_gain: 0.0,
            },
            48_000,
        );
        let mut peak = 0.0_f32;
        for frame in 0..48_000 {
            let input = if frame == 0 {
                StereoFrame::splat(1.0)
            } else {
                StereoFrame::default()
            };
            let output = delay.process(input, 1.0);
            assert!(output.left.is_finite() && output.right.is_finite());
            if frame > 480 {
                peak = peak.max(output.left.abs()).max(output.right.abs());
            }
        }
        assert!(peak > 0.0 && peak <= 1.0);
    }

    #[test]
    fn effect_controls_are_bounded_and_delay_targets_are_smoothed() {
        let mut controls = EffectControls::default();
        assert!(controls.set_parameter(REVERB_DECAY_INDEX, 99.0));
        assert_eq!(controls.reverb_decay, 4.0);
        assert!(controls.set_parameter(DELAY_FEEDBACK_INDEX, -99.0));
        assert_eq!(controls.delay_feedback, -0.5);
        assert!(!controls.set_parameter(99, 0.5));

        let mut delay = StereoDelay::new(
            NkiDelay {
                time_ms: 100.0,
                feedback: 0.6,
                panning: 0.5,
                damping: 0.2,
                wet_gain: 1.0,
                dry_gain: 0.0,
            },
            48_000,
        );
        controls.delay_time = 4.0;
        controls.delay_feedback = 0.2;
        controls.delay_damping = 0.5;
        controls.delay_stereo = 1.5;
        delay.set_controls(controls);
        assert_eq!(delay.delay_samples.target, 19_200.0);
        assert_eq!(delay.feedback.target, 0.8);
        assert_eq!(delay.crossfeed.target, 0.75);
        assert_ne!(
            delay.damping_coefficient.current,
            delay.damping_coefficient.target
        );

        let first = delay.delay_samples.next();
        assert!(first > 4_800.0 && first < 19_200.0);
    }

    #[test]
    fn low_pass_rejects_invalid_rates_and_stays_finite() {
        assert!(
            StereoBiquad::new(
                NkiFilter::LowPass2 {
                    cutoff: 0.1,
                    resonance: 0.0
                },
                0,
            )
            .is_none()
        );
        let mut filter = StereoBiquad::new(
            NkiFilter::LowPass2 {
                cutoff: 0.1,
                resonance: 0.0,
            },
            48_000,
        )
        .unwrap();
        for _ in 0..10_000 {
            assert!(
                filter
                    .process([1.0, -1.0])
                    .iter()
                    .all(|sample| sample.is_finite())
            );
        }
    }

    #[test]
    fn high_pass_two_has_the_expected_slope_and_cutoff() {
        let coefficients = high_pass(1_000.0, 0.707, 48_000.0).unwrap();
        let deep_bass = response_magnitude(coefficients, 100.0, 48_000.0);
        let cutoff = response_magnitude(coefficients, 1_000.0, 48_000.0);
        let treble = response_magnitude(coefficients, 10_000.0, 48_000.0);

        assert!(deep_bass < 0.011, "100 Hz leaked at {deep_bass}");
        assert!(
            (cutoff - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.002,
            "cutoff magnitude was {cutoff}"
        );
        assert!(treble > 0.999, "10 kHz passed at only {treble}");

        let mut processor = StereoBiquad::new(
            NkiFilter::HighPass2 {
                cutoff: 0.05,
                resonance: 0.0,
            },
            48_000,
        )
        .unwrap();
        for _ in 0..48_000 {
            assert!(
                processor
                    .process([1.0, -1.0])
                    .iter()
                    .all(|sample| sample.is_finite())
            );
        }
        let settled = processor.process([1.0, -1.0]);
        assert!(settled[0].abs() < 1.0e-5 && settled[1].abs() < 1.0e-5);
    }
}
