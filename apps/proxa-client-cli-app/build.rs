use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// copy the AI model into the build directory

fn main() {
	let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
	let root_dir = Path::new(&manifest_dir).parent().unwrap().parent().unwrap();
	let models_dir = root_dir.join("models");

	println!("cargo:rerun-if-changed={}", models_dir.display());

	let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

	let mut target_dir = out_dir.clone();
	while target_dir.file_name().unwrap() != "build" {
		target_dir.pop();
	}
	target_dir.pop();

	let build_models_dir = target_dir.join("models");

	if !build_models_dir.exists() {
		fs::create_dir_all(&build_models_dir).expect("failed to create models dir in target");
	}

	fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
		fs::create_dir_all(&dst)?;
		for entry in fs::read_dir(src)? {
			let entry = entry?;
			let ty = entry.file_type()?;
			if ty.is_dir() {
				copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
			} else {
				let src_path = entry.path();
				let dst_path = dst.as_ref().join(entry.file_name());

				let src_meta = fs::metadata(&src_path)?;
				let src_mtime = src_meta.modified()?;

				let should_copy = if let Ok(dest_meta) = fs::metadata(&dst_path) {
					dest_meta.modified()? < src_mtime
				} else {
					true
				};

				if should_copy {
					fs::copy(src_path, dst_path)?;
				}
			}
		}
		Ok(())
	}

	if models_dir.exists() {
		println!("cargo:warning=Updating models in target folder...");
		if let Err(e) = copy_dir_all(&models_dir, &build_models_dir) {
			println!("cargo:warning=Failed to copy models: {}", e);
		}
	}
}
