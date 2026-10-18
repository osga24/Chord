use crate::api::{ApiImpl, AppResult};
use crate::app::AppHandleExt;

pub async fn toggle_caps_lock_switches_input_source(api: ApiImpl) -> AppResult<()> {
    let handle = api.handle()?;
    let settings = handle.app_state().settings();
    Ok(settings.toggle_caps_lock_switches_input_source()?)
}
