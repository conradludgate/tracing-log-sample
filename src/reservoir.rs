pub(crate) struct Reservoir {
    count: usize,
    events: Box<[Vec<u8>]>,
}

impl Reservoir {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            count: 0,
            events: vec![Vec::new(); capacity].into_boxed_slice(),
        }
    }

    pub(crate) fn sample(&mut self, event: Vec<u8>) -> Vec<u8> {
        self.count += 1;

        if let Some(e) = self.events.get_mut(self.count - 1) {
            std::mem::replace(e, event)
        } else if let Some(slot) = self.events.get_mut(fastrand::usize(0..self.count)) {
            std::mem::replace(slot, event)
        } else {
            event
        }
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = Vec<u8>> + '_ {
        let iter = self.events.iter_mut().map(std::mem::take).take(self.count);
        self.count = 0;
        iter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underfull() {
        let mut reservoir = Reservoir::new(10);
        for i in 0..5 {
            let event = format!("event-{i}\n").into_bytes();
            assert!(reservoir.sample(event).is_empty());
        }
        assert_eq!(reservoir.count, 5);
        let drained: Vec<_> = reservoir.drain().collect();
        assert_eq!(drained.len(), 5);
    }

    #[test]
    fn overfull_returns_ejected() {
        let mut reservoir = Reservoir::new(10);
        let mut ejected_count = 0;
        for i in 0..1000 {
            let event = format!("event-{i}\n").into_bytes();
            if !reservoir.sample(event).is_empty() {
                ejected_count += 1;
            }
        }
        assert_eq!(reservoir.count, 1000);
        assert_eq!(ejected_count, 990);
        let drained: Vec<_> = reservoir.drain().collect();
        assert_eq!(drained.len(), 10);
    }
}
