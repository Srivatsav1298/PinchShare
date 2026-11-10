//! Pinch recognition from raw magnify deltas (platform independent, unit-tested).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pinch {
    /// Fingers together: copy into the ready slot.
    In,
    /// Fingers apart: paste here.
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Began,
    Changed,
    Ended,
}

pub struct Recognizer {
    threshold: f64,
    acc: f64,
    fired: bool,
}

impl Recognizer {
    pub fn new(threshold: f64) -> Recognizer {
        Recognizer { threshold, acc: 0.0, fired: false }
    }

    pub fn set_threshold(&mut self, t: f64) {
        self.threshold = t;
    }

    /// Feed one magnify event. Fires at most once per physical gesture, as soon as the
    /// accumulated scale change passes the threshold (so paste feels instant).
    pub fn feed(&mut self, magnification: f64, phase: Phase) -> Option<Pinch> {
        if phase == Phase::Began {
            self.acc = 0.0;
            self.fired = false;
        }
        if !self.fired {
            self.acc += magnification;
            if self.acc <= -self.threshold {
                self.fired = true;
                return Some(Pinch::In);
            }
            if self.acc >= self.threshold {
                self.fired = true;
                return Some(Pinch::Out);
            }
        }
        if phase == Phase::Ended {
            self.acc = 0.0;
            self.fired = false;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(r: &mut Recognizer, deltas: &[f64]) -> Vec<Pinch> {
        let mut out = vec![];
        for (i, d) in deltas.iter().enumerate() {
            let ph = if i == 0 { Phase::Began } else if i == deltas.len() - 1 { Phase::Ended } else { Phase::Changed };
            out.extend(r.feed(*d, ph));
        }
        out
    }

    #[test]
    fn pinch_in_fires_once() {
        let mut r = Recognizer::new(0.35);
        assert_eq!(run(&mut r, &[-0.1, -0.1, -0.1, -0.1, -0.1, -0.05]), vec![Pinch::In]);
    }

    #[test]
    fn pinch_out_fires_once() {
        let mut r = Recognizer::new(0.35);
        assert_eq!(run(&mut r, &[0.2, 0.2, 0.2, 0.2, 0.1]), vec![Pinch::Out]);
    }

    #[test]
    fn small_or_wobbly_gestures_are_ignored() {
        let mut r = Recognizer::new(0.35);
        assert!(run(&mut r, &[0.05, -0.05, 0.08, -0.07, 0.1, -0.1]).is_empty());
        assert!(run(&mut r, &[0.1, 0.1, 0.05]).is_empty());
    }

    #[test]
    fn state_resets_between_gestures() {
        let mut r = Recognizer::new(0.35);
        assert_eq!(run(&mut r, &[-0.2, -0.2]), vec![Pinch::In]);
        assert_eq!(run(&mut r, &[0.2, 0.2]), vec![Pinch::Out]);
        // Leftover accumulation from a short gesture must not leak into the next.
        assert!(run(&mut r, &[0.3, 0.0]).is_empty());
        assert!(run(&mut r, &[0.3, 0.0]).is_empty());
    }
}
