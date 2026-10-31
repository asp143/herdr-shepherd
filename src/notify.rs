use crate::herdr::Herdr;

pub struct Notifier<'a> {
    herdr: &'a dyn Herdr,
    enabled: bool,
}

impl<'a> Notifier<'a> {
    pub fn new(herdr: &'a dyn Herdr, enabled: bool) -> Self {
        Self { herdr, enabled }
    }

    pub fn send(&self, title: &str, body: &str) {
        if self.enabled
            && let Err(error) = self.herdr.notify(title, body)
        {
            eprintln!("shepherd: notification failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;

    #[test]
    fn notifier_disabled_sends_nothing() {
        let herdr = FakeHerdr::new(vec![]);
        Notifier::new(&herdr, false).send("title", "body");
        assert!(herdr.calls.borrow().is_empty());
    }

    #[test]
    fn notifier_swallows_errors() {
        let herdr = FakeHerdr::new(vec![]);
        herdr.notify_error.set(true);
        Notifier::new(&herdr, true).send("title", "body");
        assert_eq!(*herdr.calls.borrow(), ["notify title body"]);
    }
}
