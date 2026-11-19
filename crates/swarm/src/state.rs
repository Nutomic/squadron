use std::path::PathBuf;
use store::Store;

pub struct AppState {
    pub store: Store,
    pub fixtures: bool,
    pub static_dir: PathBuf,
}
