use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Hdc {
    executable: PathBuf,
}

impl Hdc {
    pub fn locate() -> Result<Self> {
        if let Some(path) = env::var_os("HDC") {
            let executable = PathBuf::from(path);
            if executable.is_file() {
                return Ok(Self { executable });
            }
        }

        if let Some(path) = default_windows_hdc() {
            if path.is_file() {
                return Ok(Self { executable: path });
            }
        }

        Ok(Self {
            executable: PathBuf::from("hdc"),
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn targets(&self) -> Result<Vec<String>> {
        let output = self.run(["--", "list", "targets"], None)?;
        Ok(parse_targets(&output_text(&output)))
    }

    pub fn list_targets(&self) -> Result<String> {
        let output = self.run(["list", "targets", "-v"], None)?;
        Ok(output_text(&output))
    }

    pub fn shell<I, S>(&self, serial: &str, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = vec!["-t".to_string(), serial.to_string(), "shell".to_string()];
        command.extend(
            args.into_iter()
                .map(|value| value.as_ref().to_string_lossy().into_owned()),
        );
        let output = self.run(command, None)?;
        if !output.status.success() {
            bail!(
                "hdc shell failed: {}\n{}",
                output.status,
                output_text(&output)
            );
        }
        Ok(output_text(&output))
    }

    pub fn file_recv(&self, serial: &str, remote: &str, local: &Path) -> Result<()> {
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let output = self.run(
            [
                "-t".to_string(),
                serial.to_string(),
                "file".to_string(),
                "recv".to_string(),
                remote.to_string(),
                local.display().to_string(),
            ],
            None,
        )?;
        if !output.status.success() {
            bail!("hdc file recv failed: {}", output_text(&output));
        }
        Ok(())
    }

    fn run<I, S>(&self, args: I, cwd: Option<&Path>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.executable);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command.output().with_context(|| {
            format!(
                "failed to execute {}; set HDC to the full hdc path",
                self.executable.display()
            )
        })
    }
}

fn default_windows_hdc() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        Some(PathBuf::from(
            r"C:\Huawei\DevEco Studio\sdk\default\openharmony\toolchains\hdc.exe",
        ))
    } else {
        None
    }
}

fn output_text(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    text
}

fn parse_targets(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(first) = line.split_whitespace().next() else {
            continue;
        };
        if first == "[Empty]" || first.to_ascii_lowercase().starts_with("empty") {
            continue;
        }
        if !result.iter().any(|item| item == first) {
            result.push(first.to_string());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::parse_targets;

    #[test]
    fn empty_target_list_is_ignored() {
        assert!(parse_targets("[Empty]\thdc\n").is_empty());
    }

    #[test]
    fn target_output_is_deduplicated() {
        let text = "ABC123\tUSB\tConnected\nABC123\tUSB\tConnected\nDEF456\tTCP\tConnected\n";
        assert_eq!(parse_targets(text), vec!["ABC123", "DEF456"]);
    }
}
