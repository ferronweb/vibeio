mod driver;
mod executor;
mod op;
mod task;

use executor::{new_runtime, yield_now};

use crate::{driver::AnyDriver, executor::spawn};

fn main() {
    let runtime = new_runtime(AnyDriver::new_mock());

    runtime.block_on(async {
        for id in 1..=3 {
            spawn(async move {
                println!("background task {id}: done");
            });
            println!("main task {id}: done");
            yield_now().await;
        }
    });
}
