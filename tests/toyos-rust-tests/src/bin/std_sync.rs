use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

fn test_mutex() {
    let data = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();

    for t in 0..4u32 {
        let data = Arc::clone(&data);
        handles.push(thread::spawn(move || {
            for i in 0..100u32 {
                data.lock().unwrap().push(t * 100 + i);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let v = data.lock().unwrap();
    assert_eq!(v.len(), 400, "expected 400 items, got {}", v.len());

    // Verify all values present
    let mut sorted = v.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 400, "expected 400 unique values");
    println!("  mutex: ok");
}

fn test_condvar() {
    let pair = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));

    let consumer = {
        let pair = Arc::clone(&pair);
        thread::spawn(move || {
            let mut received = Vec::new();
            for _ in 0..10 {
                let (lock, cvar) = &*pair;
                let mut queue = lock.lock().unwrap();
                while queue.is_empty() {
                    queue = cvar.wait(queue).unwrap();
                }
                received.push(queue.pop_front().unwrap());
            }
            received
        })
    };

    let (lock, cvar) = &*pair;
    for i in 0..10i32 {
        lock.lock().unwrap().push_back(i);
        cvar.notify_one();
        thread::sleep(Duration::from_millis(1));
    }

    let received = consumer.join().unwrap();
    assert_eq!(received, (0..10).collect::<Vec<_>>());
    println!("  condvar: ok");
}

fn test_park_unpark() {
    let t = thread::spawn(|| {
        thread::park();
        42
    });

    thread::sleep(Duration::from_millis(50));
    t.thread().unpark();
    let result = t.join().unwrap();
    assert_eq!(result, 42);
    println!("  park/unpark: ok");
}

fn test_rwlock() {
    let data = Arc::new(RwLock::new(0u64));
    let mut handles = Vec::new();

    // 4 reader threads
    for _ in 0..4 {
        let data = Arc::clone(&data);
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                let _val = *data.read().unwrap();
                thread::yield_now();
            }
        }));
    }

    // 1 writer thread
    {
        let data = Arc::clone(&data);
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                *data.write().unwrap() += 1;
                thread::yield_now();
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(*data.read().unwrap(), 100);
    println!("  rwlock: ok");
}

fn main() {
    test_mutex();
    test_condvar();
    test_park_unpark();
    test_rwlock();
    println!("all sync tests passed");
}
