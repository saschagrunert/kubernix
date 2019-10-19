#![allow(dead_code)]
use failure::{bail, format_err, Fallible};
use std::{
    env::{current_dir, split_paths, var, var_os},
    fmt::Display,
    fs::{canonicalize, create_dir_all, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

const TIMEOUT: u64 = 600;
pub const SUDO: &str = "sudo";

pub fn run_podman_test(test: &str, args: Option<&[&str]>) -> Fallible<()> {
    run_container_test(test, "podman", args)
}

pub fn run_docker_test(test: &str, args: Option<&[&str]>) -> Fallible<()> {
    run_container_test(test, "docker", args)
}

fn run_container_test(test: &str, command: &str, args: Option<&[&str]>) -> Fallible<()> {
    let image = var("IMAGE")?;
    let mut full_args = vec![
        command,
        "run",
        "--name",
        test,
        "--rm",
        "--privileged",
        "--net=host",
    ];

    // Mount /dev/mapper if needed
    let devmapper = PathBuf::from("/").join("dev").join("mapper");
    let devmapper_arg = format!("-v={d}:{d}", d = devmapper.display());
    if devmapper.exists() {
        full_args.push(&devmapper_arg);
    }

    // Mount test dir
    let mut test_dir = test_dir(test);
    create_dir_all(&test_dir)?;
    test_dir = canonicalize(&test_dir)?;
    let test_dir_volume_arg = format!("-v={d}:/kubernix-run", d = test_dir.display());
    full_args.push(&test_dir_volume_arg);

    full_args.push(&image);
    if let Some(a) = args {
        full_args.extend(a);
    }
    full_args.push("--log-level=debug");

    let success = run_test(test, &full_args, none_hook)?;

    // Cleanup
    let status = Command::new(SUDO)
        .arg(command)
        .arg("rm")
        .arg("-f")
        .arg(test)
        .status()?;

    // Result evaluation
    if !success || !status.success() {
        bail!("Test failed")
    }
    Ok(())
}

pub fn run_local_test<F>(test: &str, args: Option<&[&str]>, hook: F) -> Fallible<()>
where
    F: Fn() -> Fallible<()>,
{
    let binary = current_dir()?
        .join("target")
        .join("release")
        .join("kubernix")
        .display()
        .to_string();
    let root = format!("--root={}", run_root(test).display());
    let mut full_args: Vec<&str> = vec![&binary, &root, "--log-level=debug"];
    if let Some(a) = args {
        full_args.extend(a);
    }
    let success = run_test(test, &full_args, hook)?;

    // Kill the kubernix pid
    let pid_file = run_root(test).join("kubernix.pid");
    println!("Killing pid: {}", pid_file.display());
    Command::new(SUDO)
        .arg("pkill")
        .arg("-F")
        .arg(&pid_file)
        .status()?;
    let cleanup_success = check_file_for_output(test, "Cleanup done", "died unexpectedly")?;

    // Results evaluation
    if !success || !cleanup_success {
        bail!("Test failed")
    }
    Ok(())
}

pub fn run_root(test: &str) -> PathBuf {
    test_dir(test).join("run")
}

pub fn none_hook() -> Fallible<()> {
    Ok(())
}

fn run_test<F>(test: &str, args: &[&str], hook: F) -> Fallible<bool>
where
    F: Fn() -> Fallible<()>,
{
    // Prepare the logs dir
    let test_dir = test_dir(test);
    Command::new(SUDO)
        .arg("rm")
        .arg("-rf")
        .arg(&test_dir)
        .status()?;
    create_dir_all(&test_dir)?;
    let log_file = log_file(test);
    println!("Writing to log file: {}", log_file.display());

    // Start the process
    println!("running: {}", args.join(" "));
    let out_file = File::create(&log_file)?;
    let err_file = out_file.try_clone()?;
    Command::new(SUDO)
        .arg("env")
        .arg(format!("PATH={}", var("PATH")?))
        .args(args)
        .arg("--no-shell")
        .stderr(Stdio::from(err_file))
        .stdout(Stdio::from(out_file))
        .spawn()?;

    // Check the expected output
    println!("Waiting for process to be ready");
    let success_ready = check_file_for_output(
        test,
        "Everything is up and running",
        "Unable to start all processes",
    )?;
    println!("Process ready: {}", success_ready);

    // Run the test hook
    let success_hook = if success_ready {
        if let Err(e) = hook() {
            println!("Hook errored: {}", e);
            false
        } else {
            true
        }
    } else {
        false
    };

    // Check results
    Ok(success_ready && success_hook)
}

fn check_file_for_output(
    test: &str,
    success_pattern: &str,
    failure_pattern: &str,
) -> Fallible<bool> {
    let mut success = false;
    let now = Instant::now();
    let mut reader = BufReader::new(File::open(log_file(test))?);

    while now.elapsed().as_secs() < TIMEOUT {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if !line.is_empty() {
            print!("{}", line);
            if line.contains(success_pattern) {
                success = true;
                break;
            }
            if line.contains(failure_pattern) {
                break;
            }
        }
    }
    return Ok(success);
}

fn test_dir(test: &str) -> PathBuf {
    PathBuf::from(format!("test-{}", test))
}

fn log_file(test: &str) -> PathBuf {
    test_dir(test).join("kubernix.log")
}
