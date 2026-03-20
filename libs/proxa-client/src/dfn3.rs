use anyhow::Result;
use std::path::Path;

// DFN3 Model Sessions for DeepFilterNet
#[derive(Clone)]
pub struct DfEngine(pub std::sync::Arc<parking_lot::Mutex<df::tract::DfTract>>);
unsafe impl Send for DfEngine {} // DfTract is Send but we wrap it to be sure
unsafe impl Sync for DfEngine {}

pub static GLOBAL_DFN3_ENGINE: parking_lot::Mutex<Option<(std::path::PathBuf, DfEngine)>> =
	parking_lot::const_mutex(None);

pub fn load_dfn3_engine_internal(path: &Path) -> Result<df::tract::DfTract> {
	let tar_path = if path.is_file() {
		path.to_path_buf()
	} else {
		// check for the .tar.gz in the directory
		let mut found = None;
		for entry in std::fs::read_dir(path)? {
			let entry = entry?;
			if entry.file_name().to_string_lossy().ends_with(".tar.gz") {
				found = Some(entry.path());
				break;
			}
		}
		match found {
			Some(p) => p,
			None => {
				anyhow::bail!(
					"no .tar.gz dfn model found in {:?}, please include DeepFilterNet3_ll_onnx.tar.gz",
					path
				);
			}
		}
	};

	use df::tract::{DfParams, DfTract, RuntimeParams};
	let df_params = DfParams::new(tar_path)?;
	let mut rt_params = RuntimeParams::default();
	rt_params.n_ch = 1;
	// post_filter trends to create really odd metalic like sounding artifacts, as such we disable it
	rt_params.post_filter = false;
	// we slightly tone down the noise reduction
	// i usually do this when processing my mic so things don't fully cut out, though more fine adjustment to this may be needed
	rt_params.atten_lim_db = 80.0;

	DfTract::new(df_params, &rt_params)
}
