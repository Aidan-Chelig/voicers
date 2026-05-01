use super::{
    rms_level, AdaptiveNoiseGate, HighPassFilter, VoiceProcessorChain, GATE_CLOSE_MULTIPLIER,
    GATE_FLOOR_GAIN, GATE_GAIN_ATTACK, GATE_GAIN_RELEASE, GATE_OPEN_MULTIPLIER, HPF_ALPHA,
    NOISE_FLOOR_ATTACK, NOISE_FLOOR_DECAY,
};

impl VoiceProcessorChain {
    pub(super) fn new() -> Self {
        Self {
            high_pass: HighPassFilter::new(HPF_ALPHA),
            noise_gate: AdaptiveNoiseGate::new(),
        }
    }

    pub(super) fn reset(&mut self) {
        self.high_pass.reset();
        self.noise_gate.reset();
    }

    pub(super) fn process_frame(&mut self, frame: &mut [f32]) {
        self.high_pass.process(frame);
        self.noise_gate.process(frame);
    }
}

impl HighPassFilter {
    fn new(alpha: f32) -> Self {
        Self {
            alpha,
            prev_input: 0.0,
            prev_output: 0.0,
        }
    }

    fn reset(&mut self) {
        self.prev_input = 0.0;
        self.prev_output = 0.0;
    }

    fn process(&mut self, frame: &mut [f32]) {
        for sample in frame {
            let output = self.alpha * (self.prev_output + *sample - self.prev_input);
            self.prev_input = *sample;
            self.prev_output = output;
            *sample = output;
        }
    }
}

impl AdaptiveNoiseGate {
    fn new() -> Self {
        Self {
            noise_floor_rms: 0.0015,
            gain: 1.0,
        }
    }

    fn reset(&mut self) {
        self.noise_floor_rms = 0.0015;
        self.gain = 1.0;
    }

    fn process(&mut self, frame: &mut [f32]) {
        let rms = rms_level(frame);
        let noise_update = if rms < self.noise_floor_rms {
            NOISE_FLOOR_ATTACK
        } else {
            NOISE_FLOOR_DECAY
        };
        self.noise_floor_rms += (rms - self.noise_floor_rms) * noise_update;

        let open_threshold = (self.noise_floor_rms * GATE_OPEN_MULTIPLIER).max(0.003);
        let close_threshold = (self.noise_floor_rms * GATE_CLOSE_MULTIPLIER).max(0.002);
        let target_gain = if rms >= open_threshold {
            1.0
        } else if rms <= close_threshold {
            GATE_FLOOR_GAIN
        } else {
            let span = (open_threshold - close_threshold).max(f32::EPSILON);
            let ratio = (rms - close_threshold) / span;
            GATE_FLOOR_GAIN + (1.0 - GATE_FLOOR_GAIN) * ratio
        };

        let smoothing = if target_gain > self.gain {
            GATE_GAIN_ATTACK
        } else {
            GATE_GAIN_RELEASE
        };
        self.gain += (target_gain - self.gain) * smoothing;

        for sample in frame {
            *sample *= self.gain;
        }
    }
}
