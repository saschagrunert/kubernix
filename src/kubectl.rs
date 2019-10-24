use anyhow::{bail, Result};
use log::{debug, trace};
use std::{
    path::Path,
    process::{Command, Output},
};

pub struct Kubectl;

impl Kubectl {
    /// Run a generic kubectl command
    pub fn execute(kubeconfig: &Path, args: &[&str]) -> Result<Output> {
        let output = Command::new("kubectl")
            .args(args)
            .arg("--kubeconfig")
            .arg(kubeconfig)
            .output()?;
        if !output.status.success() {
            trace!("kubectl args: {:?}", args);
            debug!("kubectl stdout: {}", String::from_utf8(output.stdout)?);
            debug!("kubectl stderr: {}", String::from_utf8(output.stderr)?);
            bail!("kubectl command failed");
        }
        Ok(output)
    }

    /// Run kubectl config
    pub fn config(kubeconfig: &Path, args: &[&str]) -> Result<()> {
        let mut final_args = vec!["config"];
        final_args.extend(args);
        Self::execute(kubeconfig, &final_args)?;
        Ok(())
    }

    /// Run kubectl apply
    pub fn apply(kubeconfig: &Path, file: &Path) -> Result<()> {
        let file_arg = file.display().to_string();
        let args = &["apply", "-f", &file_arg];
        Self::execute(kubeconfig, args)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn execute_success() -> Result<()> {
        let k = PathBuf::from("");
        Kubectl::execute(&k, &[])?;
        Ok(())
    }
}
