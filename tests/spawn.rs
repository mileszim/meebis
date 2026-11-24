use std::io::{Read, Write};
use std::net::TcpStream;

#[test]
fn spawned_server_answers() {
    let addr = meebis::spawn().expect("spawn");
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut reply = [0u8; 7];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"+PONG\r\n");
}
