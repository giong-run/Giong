//! Many streams on one connection.

mod common;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use common::*;
use giong_grpc::{CallOptions, Channel};

#[test]
fn every_stream_of_a_connection_makes_progress() {
    giong::block_on(async {
        let client = TestServiceClient::new(Channel::new(&serve(test_server())).unwrap());
        let counts: Vec<Rc<Cell<u32>>> = (0..16).map(|_| Rc::new(Cell::new(0))).collect();
        let tasks: Vec<_> = counts
            .iter()
            .map(|count| {
                let (client, count) = (client.clone(), count.clone());
                giong::spawn(async move {
                    let (mut requests, mut responses) = client.full_duplex_call(CallOptions::new());
                    let parameters = [ResponseParameters {
                        size: 100,
                        ..Default::default()
                    }];
                    loop {
                        requests
                            .send(&StreamingOutputCallRequest {
                                response_parameters: &parameters,
                                ..Default::default()
                            })
                            .await
                            .unwrap();
                        responses.message().await.unwrap().unwrap();
                        count.set(count.get() + 1);
                    }
                })
            })
            .collect();
        giong::time::sleep(Duration::from_millis(500)).await;
        let seen: Vec<u32> = counts.iter().map(|c| c.get()).collect();
        for task in tasks {
            task.abort();
        }
        let (least, most) = (*seen.iter().min().unwrap(), *seen.iter().max().unwrap());
        assert!(
            least > 0 && most < least * 4 + 50,
            "round trips of each stream: {seen:?}"
        );
    });
}
