use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use voicers_core::MediaStreamState;

use super::{
    EncodedPacket, JitterBuffer, PlayoutDecision, DRIFT_REBASE_THRESHOLD_FRAMES, FRAME_DURATION_MS,
    FRAME_SAMPLES, FRAME_SAMPLES_U64, JITTER_MAX_DEPTH, JITTER_MAX_TARGET_DEPTH, JITTER_MIN_DEPTH,
    JITTER_TARGET_DEPTH, PLAYOUT_LATE_GRACE_MS, STABLE_PLAYOUT_TICKS_BEFORE_SHRINK,
};

impl JitterBuffer {
    pub(super) fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            next_playout_timestamp_samples: None,
            next_playout_deadline: None,
            target_depth: JITTER_TARGET_DEPTH,
            stable_playout_ticks: 0,
            late_packets: 0,
            lost_packets: 0,
            concealed_frames: 0,
            drift_corrections: 0,
        }
    }

    pub(super) fn insert(&mut self, packet: EncodedPacket) {
        if let Some(next_playout) = self.next_playout_timestamp_samples {
            if packet.timestamp_samples < next_playout {
                self.late_packets += 1;
                self.raise_target_depth();
                return;
            }
        }

        self.pending
            .entry(packet.timestamp_samples)
            .or_insert(packet);

        while self.pending.len() > JITTER_MAX_DEPTH {
            let Some((&oldest, _)) = self.pending.first_key_value() else {
                break;
            };
            if Some(oldest) == self.next_playout_timestamp_samples {
                break;
            }
            self.pending.remove(&oldest);
            self.drift_corrections += 1;
        }
    }

    pub(super) fn next_playout_decision(&mut self, now: Instant) -> PlayoutDecision {
        if self.next_playout_timestamp_samples.is_none() {
            if self.pending.len() < self.target_depth {
                return PlayoutDecision::Hold;
            }
            self.next_playout_timestamp_samples = self
                .pending
                .first_key_value()
                .map(|(&timestamp, _)| timestamp);
            let startup_lead = self.target_depth.saturating_sub(1) as u32;
            let startup_offset =
                Duration::from_millis(u64::from(FRAME_DURATION_MS) * u64::from(startup_lead));
            self.next_playout_deadline = now.checked_sub(startup_offset).or(Some(now));
            self.stable_playout_ticks = 0;
        }

        let Some(deadline) = self.next_playout_deadline else {
            return PlayoutDecision::Hold;
        };
        if now < deadline {
            return PlayoutDecision::Hold;
        }

        let expected = self
            .next_playout_timestamp_samples
            .expect("playout timestamp initialized");

        if let Some((&earliest, _)) = self.pending.first_key_value() {
            let max_gap = FRAME_SAMPLES_U64 * DRIFT_REBASE_THRESHOLD_FRAMES;
            if earliest > expected + max_gap {
                self.next_playout_timestamp_samples = Some(earliest);
                self.next_playout_deadline = Some(now);
                self.drift_corrections += 1;
            }
        }

        let expected = self
            .next_playout_timestamp_samples
            .expect("playout timestamp available after rebase");

        if let Some(packet) = self.pending.remove(&expected) {
            self.advance_playout_clock(deadline, expected);
            self.note_stable_tick();
            return PlayoutDecision::Packet(packet);
        }

        let late_grace = Duration::from_millis(PLAYOUT_LATE_GRACE_MS);
        if now < deadline + late_grace {
            return PlayoutDecision::Hold;
        }

        if self.pending.len() < self.target_depth {
            self.next_playout_timestamp_samples = None;
            self.next_playout_deadline = None;
            self.stable_playout_ticks = 0;
            return PlayoutDecision::Hold;
        }

        if let Some(recovery_packet) = self
            .pending
            .get(&(expected + FRAME_SAMPLES_U64))
            .map(|packet| packet.payload.clone())
        {
            self.advance_playout_clock(deadline, expected);
            self.concealed_frames += 1;
            self.lost_packets += 1;
            self.raise_target_depth();
            return PlayoutDecision::Fec { recovery_packet };
        }

        self.advance_playout_clock(deadline, expected);
        self.concealed_frames += 1;
        self.lost_packets += 1;
        self.raise_target_depth();
        PlayoutDecision::Conceal
    }

    pub(super) fn stream_state(&self) -> MediaStreamState {
        if self.next_playout_timestamp_samples.is_some() {
            MediaStreamState::Active
        } else if self.pending.is_empty() {
            MediaStreamState::Idle
        } else {
            MediaStreamState::Primed
        }
    }

    pub(super) fn queue_depth(&self) -> usize {
        self.pending.len()
    }

    pub(super) fn queued_samples(&self) -> usize {
        self.pending.len() * FRAME_SAMPLES
    }

    fn advance_playout_clock(&mut self, deadline: Instant, expected: u64) {
        self.next_playout_timestamp_samples = Some(expected + FRAME_SAMPLES_U64);
        self.next_playout_deadline =
            Some(deadline + Duration::from_millis(u64::from(FRAME_DURATION_MS)));
    }

    fn raise_target_depth(&mut self) {
        self.target_depth = (self.target_depth + 1).min(JITTER_MAX_TARGET_DEPTH);
        self.stable_playout_ticks = 0;
    }

    fn note_stable_tick(&mut self) {
        if self.pending.len() <= self.target_depth {
            self.stable_playout_ticks = 0;
            return;
        }

        self.stable_playout_ticks += 1;
        if self.stable_playout_ticks >= STABLE_PLAYOUT_TICKS_BEFORE_SHRINK {
            self.target_depth = self.target_depth.saturating_sub(1).max(JITTER_MIN_DEPTH);
            self.stable_playout_ticks = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        EncodedPacket, JitterBuffer, PlayoutDecision, FRAME_DURATION_MS, FRAME_SAMPLES_U64,
    };

    fn packet(sequence: u64, timestamp_samples: u64) -> EncodedPacket {
        EncodedPacket {
            timestamp_samples,
            payload: vec![sequence as u8, 0x55],
        }
    }

    fn next_frame_instant(now: Instant) -> Instant {
        now + Duration::from_millis(u64::from(FRAME_DURATION_MS))
    }

    #[test]
    fn jitter_buffer_primes_and_reorders_by_timestamp() {
        let mut jitter = JitterBuffer::new();
        let now = Instant::now();
        jitter.insert(packet(4, FRAME_SAMPLES_U64 * 3));
        jitter.insert(packet(2, FRAME_SAMPLES_U64));
        jitter.insert(packet(1, 0));
        jitter.insert(packet(3, FRAME_SAMPLES_U64 * 2));

        match jitter.next_playout_decision(now) {
            PlayoutDecision::Packet(packet) => assert_eq!(packet.payload[0], 1),
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn jitter_buffer_uses_fec_for_single_frame_gap() {
        let mut jitter = JitterBuffer::new();
        let now = Instant::now();
        jitter.insert(packet(1, 0));
        jitter.insert(packet(3, FRAME_SAMPLES_U64 * 2));
        jitter.insert(packet(4, FRAME_SAMPLES_U64 * 3));
        jitter.insert(packet(5, FRAME_SAMPLES_U64 * 4));
        jitter.insert(packet(6, FRAME_SAMPLES_U64 * 5));

        let _ = jitter.next_playout_decision(now);
        match jitter.next_playout_decision(next_frame_instant(now) + Duration::from_millis(6)) {
            PlayoutDecision::Fec { recovery_packet } => assert_eq!(recovery_packet[0], 3),
            other => panic!("unexpected decision: {other:?}"),
        }
    }

    #[test]
    fn jitter_buffer_rebases_when_gap_is_too_large() {
        let mut jitter = JitterBuffer::new();
        let now = Instant::now();
        jitter.insert(packet(1, 0));
        jitter.insert(packet(2, FRAME_SAMPLES_U64));
        jitter.insert(packet(3, FRAME_SAMPLES_U64 * 2));
        jitter.insert(packet(4, FRAME_SAMPLES_U64 * 3));

        let _ = jitter.next_playout_decision(now);
        jitter.insert(packet(20, FRAME_SAMPLES_U64 * 20));
        jitter.insert(packet(21, FRAME_SAMPLES_U64 * 21));
        jitter.insert(packet(22, FRAME_SAMPLES_U64 * 22));

        let _ = jitter.next_playout_decision(next_frame_instant(now));
        let _ = jitter.next_playout_decision(next_frame_instant(next_frame_instant(now)));
        let _ = jitter.next_playout_decision(next_frame_instant(next_frame_instant(
            next_frame_instant(now),
        )));

        match jitter.next_playout_decision(next_frame_instant(next_frame_instant(
            next_frame_instant(next_frame_instant(now)),
        ))) {
            PlayoutDecision::Packet(packet) => assert_eq!(packet.payload[0], 20),
            other => panic!("unexpected decision: {other:?}"),
        }
    }
}
