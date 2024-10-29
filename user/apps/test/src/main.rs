use libc::{fcntl, F_SETFL};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::fs::{remove_file, File};
use std::io::{Read, Result, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    // Create and write to a large file
    let filename = "large_file.txt";
    {
        let mut file = File::create(filename)?;
        for _ in 0..10_000 {
            writeln!(file, "This is a line of text.")?;
        }
    }

    // Open the file for reading
    let mut file = File::open(filename)?;
    let fd = file.as_raw_fd();

    // Ensure the read operation is blocking
    unsafe {
        fcntl(fd, F_SETFL, 0); // Set to blocking mode
    }

    // Used to store the read byte data
    let buffer = Arc::new(Mutex::new(vec![0u8; 1024])); // Use a larger buffer
    let buffer_clone = Arc::clone(&buffer);

    // Thread 1: Simulate a blocking read operation
    let handle = thread::spawn(move || {
        let mut file = unsafe { File::from_raw_fd(fd) }; // Create file from raw file descriptor
        println!("Starting blocking read operation...");
        loop {
            let mut buffer_lock = buffer_clone.lock().unwrap();
            match file.read(&mut *buffer_lock) {
                Ok(0) => {
                    println!("No more data to read, exiting read");
                    break;
                }
                Ok(n) => {
                    // Output the number of bytes read, occupying the entire line
                    //if n != 1024 {
                        let output = format!("Read {} bytes", n);
                        println!("{:<width$}", output, width = 80); // Adjust width as needed
                    //}
                }
                Err(e) => {
                    eprintln!("Read failed: {:?}", e);
                    break;
                }
            }
        }
    });

    // Thread 2: Delay sending SIGINT (Ctrl-C) signal to interrupt the blocking `read`
    thread::sleep(Duration::new(0, 5_0000));
    println!("Sending SIGINT (Ctrl-C) signal to interrupt read operation...");
    let pid = Pid::this();
    kill(pid, Signal::SIGINT).expect("Failed to send signal");

    handle.join().expect("Error in read thread");

    // Delete the temporary file
    remove_file(filename).expect("Failed to delete file");

    Ok(())
}
