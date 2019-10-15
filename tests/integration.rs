use failure::{bail, Fallible};
use std::{
    env::current_dir,
    fs::{create_dir_all, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

const TIMEOUT: u64 = 120;
const PATTERN: &str = "Spawning interactive shell";

fn test(run: usize, args: &[&str]) -> Fallible<()> {
    // Prepare the log
    let test_dir = PathBuf::from(format!("kubernix-run-test-{}", run));
    Command::new("sudo")
        .arg("rm")
        .arg("-rf")
        .arg(&test_dir)
        .status()?;
    create_dir_all(&test_dir)?;
    let file_path = test_dir.join(format!("kubernix-{}.log", run));
    println!("Writing to log file: {}", file_path.display());

    // Start the process
    let out_file = File::create(&file_path)?;
    let err_file = out_file.try_clone()?;
    let child = Command::new("sudo")
        .arg("-E")
        .arg(
            current_dir()?
                .join("target")
                .join("release")
                .join("kubernix"),
        )
        .arg(format!("--root={}", test_dir.join("run").display()))
        .arg("--log-level=debug")
        .args(args)
        .stderr(Stdio::from(err_file))
        .stdout(Stdio::from(out_file))
        .spawn()?;

    // Check the expected output
    let mut reader = BufReader::new(File::open(&file_path)?);
    let mut success = false;
    let now = Instant::now();
    while now.elapsed().as_secs() < TIMEOUT {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if !line.is_empty() {
            print!("{}", line);
        }
        if line.contains(PATTERN) {
            success = true;
            break;
        }
    }

    // Cleanup
    Command::new("sudo")
        .arg("kill")
        .arg(child.id().to_string())
        .status()?;
    if !success {
        bail!("Unable to find pattern {} in output", PATTERN);
    }
    Ok(())
}

#[test]
fn single_node() -> Fallible<()> {
    test(0, &[])
}

#[test]
fn multi_node() -> Fallible<()> {
    test(1, &["--nodes=2"])
}
