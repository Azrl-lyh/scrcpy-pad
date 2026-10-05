use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::hdc::Hdc;

pub fn capture(hdc: &Hdc, serial: &str, local_output: &str) -> Result<PathBuf> {
    let remote = "/data/local/tmp/scrcpy_pad_screen.png";
    let _ = hdc.shell(serial, ["rm", "-f", remote]);
    let output = hdc.shell(serial, ["uitest", "screenCap", "-p", remote])?;
    if !output.trim().is_empty() {
        println!("{}", output.trim());
    }

    let local = Path::new(local_output).to_path_buf();
    hdc.file_recv(serial, remote, &local)?;
    if !local.is_file() {
        bail!(
            "capture command returned success but {} does not exist",
            local.display()
        );
    }
    std::fs::metadata(&local).with_context(|| format!("stat {}", local.display()))?;
    Ok(local)
}
