use myko::myko_item;
use myko::prelude::{EventPublishing as _, Querying as _, RegistryScoped as _, RequestScoped as _};

use crate::DispatchHarness;

/// A currently connected dispatch host and the harnesses it opted into.
#[myko_item]
pub struct DispatchHost {
    pub launch_harnesses: Vec<DispatchHarness>,
}
