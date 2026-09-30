use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() {
    println!("App started");

    let task1 = tokio::spawn(async {
        loop {
            println!("Task 1: listening for events...");
            sleep(Duration::from_secs(2)).await;
        }
    });

    let task2 = tokio::spawn(async {
        loop {
            println!("Task 2: doing other work...");
            sleep(Duration::from_secs(3)).await;
        }
    });

    // Keep main alive until the tasks finish.
    // These loops never finish in this example.
    let _ = tokio::join!(task1, task2);
}