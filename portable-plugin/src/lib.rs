use rackforge_plugin_sdk::{MidiEvent, ParameterEvent, Processor, export_processor};
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};
use std::io::Cursor;
use std::sync::Arc;

const RESOURCE_ID: &str = "factory-soundfont";
const PRESET_ID: &str = "factory.ydp-grand-piano";
const MAX_BANK_BYTES: usize = 128 * 1024 * 1024;
const MAX_FRAMES: usize = 4096;

struct PendingResource {
    expected_bytes: usize,
    bytes: Vec<u8>,
}

struct PortableSoundfonts {
    sound_font: Option<Arc<SoundFont>>,
    synth: Option<Synthesizer>,
    pending: Option<PendingResource>,
    sample_rate: i32,
    left: Vec<f32>,
    right: Vec<f32>,
}

impl Default for PortableSoundfonts {
    fn default() -> Self {
        Self {
            sound_font: None,
            synth: None,
            pending: None,
            sample_rate: 48_000,
            left: vec![0.0; MAX_FRAMES],
            right: vec![0.0; MAX_FRAMES],
        }
    }
}

impl PortableSoundfonts {
    fn rebuild_synth(&mut self) -> bool {
        let Some(sound_font) = self.sound_font.as_ref() else {
            self.synth = None;
            return true;
        };
        let mut settings = SynthesizerSettings::new(self.sample_rate);
        settings.block_size = 64;
        settings.maximum_polyphony = 96;
        settings.enable_reverb_and_chorus = false;
        match Synthesizer::new(sound_font, &settings) {
            Ok(mut synth) => {
                synth.set_master_volume(0.9);
                self.synth = Some(synth);
                true
            }
            Err(_) => {
                self.synth = None;
                false
            }
        }
    }

    fn dispatch_midi(synth: &mut Synthesizer, event: MidiEvent) {
        let status = event.data[0];
        if event.length == 0 || !(0x80..0xF0).contains(&status) {
            return;
        }
        synth.process_midi_message(
            i32::from(status & 0x0F),
            i32::from(status & 0xF0),
            i32::from(event.data[1] & 0x7F),
            i32::from(event.data[2] & 0x7F),
        );
    }

    fn render_segment(
        synth: &mut Synthesizer,
        left: &mut [f32],
        right: &mut [f32],
        output: &mut [f32],
        channels: usize,
        start: usize,
        end: usize,
    ) {
        let length = end.saturating_sub(start);
        if length == 0 {
            return;
        }
        synth.render(&mut left[..length], &mut right[..length]);
        for frame in 0..length {
            let target = (start + frame) * channels;
            if channels == 1 {
                output[target] = (left[frame] + right[frame]) * 0.5;
            } else {
                output[target] = left[frame];
                output[target + 1] = right[frame];
            }
        }
    }
}

impl Processor for PortableSoundfonts {
    fn prepare(
        &mut self,
        sample_rate: f64,
        maximum_frames: u32,
        _input_channels: u32,
        output_channels: u32,
    ) -> bool {
        if !sample_rate.is_finite()
            || !(16_000.0..=192_000.0).contains(&sample_rate)
            || maximum_frames as usize > MAX_FRAMES
            || !(1..=2).contains(&output_channels)
        {
            return false;
        }
        self.sample_rate = sample_rate.round() as i32;
        self.rebuild_synth()
    }

    fn set_parameter(&mut self, _index: u32, _value: f64) -> bool {
        false
    }

    fn reset(&mut self) {
        if let Some(synth) = self.synth.as_mut() {
            synth.reset();
        }
    }

    fn begin_resource(&mut self, id: &str, total_bytes: u64) -> bool {
        let Ok(expected_bytes) = usize::try_from(total_bytes) else {
            return false;
        };
        if id != RESOURCE_ID
            || expected_bytes == 0
            || expected_bytes > MAX_BANK_BYTES
            || self.pending.is_some()
        {
            return false;
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(expected_bytes).is_err() {
            return false;
        }
        self.pending = Some(PendingResource {
            expected_bytes,
            bytes,
        });
        true
    }

    fn write_resource(&mut self, offset: u64, bytes: &[u8]) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return false;
        };
        if offset != pending.bytes.len() as u64
            || pending.bytes.len().saturating_add(bytes.len()) > pending.expected_bytes
        {
            return false;
        }
        pending.bytes.extend_from_slice(bytes);
        true
    }

    fn end_resource(&mut self) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        if pending.bytes.len() != pending.expected_bytes {
            return false;
        }
        let mut source = Cursor::new(pending.bytes.as_slice());
        let Ok(sound_font) = SoundFont::new(&mut source) else {
            return false;
        };
        self.sound_font = Some(Arc::new(sound_font));
        self.rebuild_synth()
    }

    fn load_preset(&mut self, id: &str) -> bool {
        if id != PRESET_ID {
            return false;
        }
        if let Some(synth) = self.synth.as_mut() {
            synth.reset();
        }
        true
    }

    fn process(
        &mut self,
        _input: &[f32],
        output: &mut [f32],
        midi: &[MidiEvent],
        _parameters: &[ParameterEvent],
        frames: u32,
        _input_channels: u32,
        output_channels: u32,
    ) {
        output.fill(0.0);
        let frames = frames as usize;
        let channels = output_channels as usize;
        if frames > MAX_FRAMES || !(1..=2).contains(&channels) {
            return;
        }
        let Some(synth) = self.synth.as_mut() else {
            return;
        };
        let mut position = 0;
        for event in midi {
            let event_frame = (event.frame as usize).min(frames);
            Self::render_segment(
                synth,
                &mut self.left,
                &mut self.right,
                output,
                channels,
                position,
                event_frame,
            );
            Self::dispatch_midi(synth, *event);
            position = event_frame;
        }
        Self::render_segment(
            synth,
            &mut self.left,
            &mut self.right,
            output,
            channels,
            position,
            frames,
        );
    }
}

export_processor!(
    PortableSoundfonts,
    max_frames = MAX_FRAMES,
    max_input_channels = 0,
    max_output_channels = 2,
    max_midi_events = 512,
    max_parameter_events = 16,
    max_transfer_bytes = 65536
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_silent_before_the_factory_bank_arrives() {
        let mut plugin = PortableSoundfonts::default();
        assert!(plugin.prepare(48_000.0, 64, 0, 2));
        let mut output = [1.0; 128];
        plugin.process(&[], &mut output, &[], &[], 64, 0, 2);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn rejects_unknown_resources_and_presets() {
        let mut plugin = PortableSoundfonts::default();
        assert!(!plugin.begin_resource("other", 1));
        assert!(!plugin.load_preset("other"));
    }
}

