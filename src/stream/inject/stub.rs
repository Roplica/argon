use crate::stream::capture::InputEvent;

pub struct Injector;

impl Injector {
    pub fn new() -> anyhow::Result<Self> {
        anyhow::bail!("X11 input injection is not implemented on this platform")
    }

    pub fn send(&self, _event: &InputEvent) {}
}
