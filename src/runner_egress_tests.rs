use super::*;

#[test]
fn only_public_addresses_count_as_public() {
    for refused in [
        "10.20.0.66",
        "172.20.0.2",
        "192.168.1.1",
        "127.0.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "fd00::1",
        "fe80::1",
        "::ffff:192.168.1.1",
        "::10.20.0.1",
        "64:ff9b:1::a00:1",
        "2002:a00:1::1",
        "2001:0:a00:1::1",
        "fec0::1",
    ] {
        assert!(!is_public(refused.parse().unwrap()), "{refused} was public");
    }
    for public in [
        "104.16.0.1",
        "151.101.1.69",
        "2606:4700::6810:84e5",
        "::ffff:8.8.8.8",
    ] {
        assert!(is_public(public.parse().unwrap()), "{public} was refused");
    }
}

#[test]
fn reachable_means_public_and_port_80_or_443() {
    assert!(reachable("104.16.0.1:443".parse().unwrap()));
    assert!(!reachable("104.16.0.1:22".parse().unwrap()));
    assert!(!reachable("10.20.0.66:443".parse().unwrap()));
}

#[test]
fn parses_tunnels_and_rewrites_plain_requests() {
    assert_eq!(
        parse("CONNECT registry.npmjs.org:443 HTTP/1.1\r\nHost: registry.npmjs.org:443").unwrap(),
        Request {
            host: "registry.npmjs.org".into(),
            port: 443,
            forward: None
        }
    );
    assert_eq!(
        parse("CONNECT [2606:4700::1]:443 HTTP/1.1").unwrap().host,
        "2606:4700::1"
    );
    let plain = parse(
        "GET http://deb.example/dists/x?y=1 HTTP/1.1\r\nHost: deb.example\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*",
    )
    .unwrap();
    assert_eq!((plain.host.as_str(), plain.port), ("deb.example", 80));
    assert_eq!(
        plain.forward.unwrap(),
        "GET /dists/x?y=1 HTTP/1.1\r\nHost: deb.example\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    assert!(parse("CONNECT user:pw@host:443 HTTP/1.1").is_err());
    assert!(parse("CONNECT host HTTP/1.1").is_err());
    assert!(parse("GET https://host/ HTTP/1.1").is_err());
}

#[test]
fn admits_only_registered_outbound_networks() {
    let egress = Egress::default();
    egress.allow("job-a", "172.30.0.0/16").unwrap();
    assert!(egress.outbound("job-a"));
    assert_eq!(egress.admits("172.30.4.2".parse().unwrap()), Some(None));
    assert_eq!(egress.admits("172.31.0.2".parse().unwrap()), None);
    egress.forget("job-a");
    assert!(!egress.outbound("job-a"));
    assert_eq!(egress.admits("172.30.4.2".parse().unwrap()), None);
    egress
        .allow_only("fetch-a", "172.30.0.0/16", "Gitea.Example")
        .unwrap();
    assert!(egress.outbound("fetch-a"));
    assert_eq!(
        egress.admits("172.30.4.2".parse().unwrap()),
        Some(Some("gitea.example".into()))
    );
}

#[tokio::test]
async fn a_fetch_network_reaches_only_its_repository_host_on_443() {
    let egress = Egress::default();
    egress
        .allow_only("fetch", "127.0.0.0/8", "gitea.example")
        .unwrap();
    // Everything allowed by the public rule is irrelevant here.
    let address = proxy(egress, |_| true).await;
    for request in [
        "CONNECT github.com:443 HTTP/1.1\r\n\r\n",
        "CONNECT gitea.example:22 HTTP/1.1\r\n\r\n",
        "GET http://gitea.example/ HTTP/1.1\r\nHost: gitea.example\r\n\r\n",
    ] {
        let answer = exchange(address, request).await;
        assert!(answer.starts_with("HTTP/1.1 403"), "{request}: {answer}");
        assert!(answer.contains("only its repository host"), "{answer}");
    }
}

async fn proxy(egress: Egress, permitted: fn(SocketAddr) -> bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, egress, permitted));
    address
}

async fn exchange(proxy: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = vec![0u8; 4096];
    let read = stream.read(&mut answer).await.unwrap();
    String::from_utf8_lossy(&answer[..read]).into_owned()
}

#[tokio::test]
async fn refuses_a_container_without_outbound_access() {
    let address = proxy(Egress::default(), |_| true).await;
    let answer = exchange(address, "CONNECT example.com:443 HTTP/1.1\r\n\r\n").await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    assert!(answer.contains("no outbound access"));
}

#[tokio::test]
async fn refuses_a_destination_on_the_local_network() {
    let egress = Egress::default();
    egress.allow("loopback", "127.0.0.0/8").unwrap();
    let address = proxy(egress, reachable).await;
    // The production rule: the supervisor's own host is never a destination.
    let answer = exchange(address, "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n").await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    let answer = exchange(
        address,
        "GET http://192.168.1.1/ HTTP/1.1\r\nHost: 192.168.1.1\r\n\r\n",
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
}

#[tokio::test]
async fn tunnels_and_forwards_to_a_permitted_destination() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = upstream.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = upstream.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                let mut reply = b"echo:".to_vec();
                reply.extend_from_slice(&buffer[..read]);
                socket.write_all(&reply).await.unwrap();
            });
        }
    });
    let egress = Egress::default();
    egress.allow("loopback", "127.0.0.0/8").unwrap();
    // A test rule that lets the proxy reach the local echo server.
    let address = proxy(egress, |address| address.ip().is_loopback()).await;

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(format!("CONNECT {target} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut answer = vec![0u8; 4096];
    let read = stream.read(&mut answer).await.unwrap();
    assert!(String::from_utf8_lossy(&answer[..read]).starts_with("HTTP/1.1 200"));
    stream.write_all(b"hello").await.unwrap();
    let read = stream.read(&mut answer).await.unwrap();
    assert_eq!(&answer[..read], b"echo:hello");

    let answer = exchange(
        address,
        &format!(
            "GET http://{target}/path HTTP/1.1\r\nHost: x\r\nProxy-Connection: keep-alive\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(
        answer,
        "echo:GET /path HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    );
}
