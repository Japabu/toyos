use std::fs::{self, File};
use std::mem::ManuallyDrop;
use std::os::toyos::fs::symlink;
use std::os::toyos::io::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};

fn main() {
    let entries: Vec<_> = fs::read_dir("/bin")
        .expect("should be able to read /bin")
        .filter_map(|e| e.ok())
        .collect();
    assert!(!entries.is_empty(), "/bin should not be empty");

    let self_exists = std::path::Path::new("/bin/test_rs_std_fs").exists();
    assert!(self_exists, "our own binary should exist in /bin");

    let data = fs::read("/bin/test_rs_std_fs")
        .expect("should be able to read our own binary");
    assert!(!data.is_empty(), "binary should not be empty");

    // Non-existent file should return NotFound
    let err = fs::read("/bin/nonexistent").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    file_types();

    println!("all fs tests passed");
}

/// What `Metadata::file_type` answers for each kind of thing this machine has.
///
/// Every arm here is an exclusion as much as an assertion: a `FileType` that
/// answered yes to two questions at once is the shape all three defects took.
fn file_types() {
    let ty = fs::metadata("/bin/test_rs_std_fs").expect("stat our own binary").file_type();
    assert!(ty.is_file() && !ty.is_dir() && !ty.is_symlink(), "a regular file typed as {ty:?}");

    let ty = fs::metadata("/bin").expect("stat /bin").file_type();
    assert!(ty.is_dir() && !ty.is_file() && !ty.is_symlink(), "a directory typed as {ty:?}");

    pipe_is_not_a_directory();
    symlink_is_not_also_a_file();

    println!("  file types: ok");
}

/// ToyOS has no directory file type, so `stat` reported a directory's as `Pipe`
/// and `is_dir()` compared against it — which made an `fstat` of a real pipe
/// answer yes.
fn pipe_is_not_a_directory() {
    let mut child = Command::new("/bin/echo")
        .arg("down the pipe")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn echo");
    let pipe = child.stdout.take().expect("echo was given a stdout pipe");

    // The pipe still owns the descriptor; this only borrows it to reach `fstat`.
    let borrowed = ManuallyDrop::new(unsafe { File::from_raw_fd(pipe.as_raw_fd()) });
    let ty = borrowed.metadata().expect("fstat a pipe").file_type();
    assert!(!ty.is_dir(), "an fstat of a pipe answered is_dir()");
    assert!(!ty.is_file(), "an fstat of a pipe answered is_file()");
    assert!(!ty.is_symlink(), "an fstat of a pipe answered is_symlink()");

    drop(pipe);
    child.wait().expect("echo never exited");
}

/// `lstat` reported a symlink as a file *and* a symlink, which no other
/// platform does.
fn symlink_is_not_also_a_file() {
    let target = "/tmp/std_fs_symlink_target";
    let link = "/tmp/std_fs_symlink";
    fs::write(target, b"pointed at\n").expect("write the symlink's target");
    let _ = fs::remove_file(link);
    symlink(target, link).expect("create a symlink on /tmp");

    let ty = fs::symlink_metadata(link).expect("lstat the symlink").file_type();
    assert!(ty.is_symlink(), "lstat of a symlink typed it as {ty:?}");
    assert!(!ty.is_file(), "lstat of a symlink also answered is_file()");
    assert!(!ty.is_dir(), "lstat of a symlink also answered is_dir()");
}
