use failure::{bail, Fallible};
use std::{
    env::{current_dir, var},
    fs::{create_dir_all, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

const FAILURE_PATTERN: &str = "Unable to start all processes";
const SUCCESS_PATTERN: &str = "Spawning interactive shell";
const SUDO: &str = "sudo";
const TIMEOUT: u64 = 600;

#[test]
fn local_single_node() -> Fallible<()> {
    run_local_test("local-single-node", None)
}

#[test]
fn local_multi_node() -> Fallible<()> {
    run_local_test("local-multi-node", Some(&["--nodes=2"]))
}

#[test]
fn docker_single_node() -> Fallible<()> {
    run_docker_test("docker-single-node", None)
}

#[test]
fn podman_single_node() -> Fallible<()> {
    run_podman_test("podman-single-node", None)
}

#[test]
fn podman_multi_node() -> Fallible<()> {
    run_podman_test("podman-multi-node", Some(&["--nodes=2"]))
}

fn run_podman_test(test: &str, args: Option<&[&str]>) -> Fallible<()> {
    run_container_test(test, "podman", args)
}

fn run_docker_test(test: &str, args: Option<&[&str]>) -> Fallible<()> {
    run_container_test(test, "docker", args)
}

fn run_container_test(test: &str, command: &str, args: Option<&[&str]>) -> Fallible<()> {
    let image = var("IMAGE")?;
    let mut full_args = vec![command, "run", "--rm", "--privileged", "--net=host"];

    // Mount /dev/mapper if needed
    let devmapper = PathBuf::from("/").join("dev").join("mapper");
    let devmapper_arg = format!("-v={d}:{d}", d = devmapper.display().to_string());
    if devmapper.exists() {
        full_args.push(&devmapper_arg);
    }

    full_args.push(&image);
    if let Some(a) = args {
        full_args.extend(a);
    }
    run_test(test, &full_args)
}

fn run_local_test(test: &str, args: Option<&[&str]>) -> Fallible<()> {
    let binary = current_dir()?
        .join("target")
        .join("release")
        .join("kubernix")
        .display()
        .to_string();
    let root = format!("--root={}", test_dir(test).join("run").display());
    let mut full_args = vec!["-E", &binary, &root, "--log-level=debug"];
    if let Some(a) = args {
        full_args.extend(a);
    }
    run_test(test, &full_args)
}

fn run_test(test: &str, args: &[&str]) -> Fallible<()> {
    // Prepare the logs dir
    let test_dir = test_dir(test);
    Command::new(SUDO)
        .arg("rm")
        .arg("-rf")
        .arg(&test_dir)
        .status()?;
    create_dir_all(&test_dir)?;
    let log_file = test_dir.join("kubernix.log");
    println!("Writing to log file: {}", log_file.display());

    // Start the process
    println!("running: {}", args.join(" "));
    let out_file = File::create(&log_file)?;
    let err_file = out_file.try_clone()?;
    let child = Command::new(SUDO)
        .args(args)
        .stderr(Stdio::from(err_file))
        .stdout(Stdio::from(out_file))
        .spawn()?;

    // Check the expected output
    let mut reader = BufReader::new(File::open(&log_file)?);
    let mut success = false;

    let now = Instant::now();
    while now.elapsed().as_secs() < TIMEOUT {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if !line.is_empty() {
            print!("{}", line);
        }
        if line.contains(SUCCESS_PATTERN) {
            success = true;
            break;
        }
        if line.contains(FAILURE_PATTERN) {
            break;
        }
    }

    // Cleanup
    Command::new(SUDO)
        .arg("kill")
        .arg(child.id().to_string())
        .status()?;
    if !success {
        bail!("Unable to find pattern {} in output", SUCCESS_PATTERN);
    }
    Ok(())
}

fn test_dir(test: &str) -> PathBuf {
    PathBuf::from(format!("test-{}", test))
}
