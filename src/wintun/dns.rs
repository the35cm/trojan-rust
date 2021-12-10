use crate::{
    proto::MAX_PACKET_SIZE,
    wintun::{DNS_LOCAL, DNS_POISONED, DNS_TRUSTED},
    OPTIONS,
};
use crossbeam::channel::Sender;
use mio::{event::Event, net::UdpSocket, Interest, Poll, Token};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, ErrorKind},
    net::SocketAddr,
    str::FromStr,
    time::Instant,
};
use trust_dns_proto::{
    op::{Message, MessageType, Query, ResponseCode},
    rr::{DNSClass, Name, RData, Record, RecordType},
    serialize::binary::BinDecodable,
};

pub struct DnsServer {
    listener: UdpSocket,
    trusted: UdpSocket,
    poisoned: UdpSocket,
    buffer: Vec<u8>,
    arp_data: Vec<u8>,
    blocked_domains: Vec<String>,
    store: HashMap<String, QueryResult>,
    sender: Sender<String>,
}

struct QueryResult {
    addresses: Vec<SocketAddr>,
    response: Vec<u8>,
    update_time: Instant,
}

impl DnsServer {
    pub fn new(sender: Sender<String>) -> Self {
        let default_ip = "0.0.0.0:0".to_owned();

        Self {
            sender,
            listener: UdpSocket::bind("127.0.0.1:53".parse().unwrap()).unwrap(),
            trusted: UdpSocket::bind(default_ip.as_str().parse().unwrap()).unwrap(),
            poisoned: UdpSocket::bind(default_ip.as_str().parse().unwrap()).unwrap(),
            buffer: vec![0; MAX_PACKET_SIZE],
            blocked_domains: vec![],
            arp_data: vec![],
            store: HashMap::new(),
        }
    }

    pub fn setup(&mut self, poll: &Poll) {
        let trusted_dns = OPTIONS.wintun_args().trusted_dns.clone() + ":53";
        let poisoned_dns = OPTIONS.wintun_args().poisoned_dns.clone() + ":53";
        self.trusted
            .connect(trusted_dns.as_str().parse().unwrap())
            .unwrap();
        self.poisoned
            .connect(poisoned_dns.as_str().parse().unwrap())
            .unwrap();
        poll.registry()
            .register(&mut self.trusted, Token(DNS_TRUSTED), Interest::READABLE)
            .unwrap();
        poll.registry()
            .register(&mut self.poisoned, Token(DNS_POISONED), Interest::READABLE)
            .unwrap();
        poll.registry()
            .register(&mut self.listener, Token(DNS_LOCAL), Interest::READABLE)
            .unwrap();

        let file = File::open(OPTIONS.wintun_args().blocked_domain_list.as_str()).unwrap();
        let reader = BufReader::new(file);
        reader
            .lines()
            .for_each(|line| self.blocked_domains.push(line.unwrap() + "."));

        let mut message = Message::new();
        message.set_message_type(MessageType::Response);
        message.set_id(1);
        message.set_recursion_desired(true);
        message.set_recursion_available(true);
        message.set_response_code(ResponseCode::NoError);
        let mut query = Query::new();
        let name = Name::from_str("1.0.0.127.in-addr.arpa.").unwrap();
        query.set_name(name.clone());
        query.set_query_type(RecordType::PTR);
        query.set_query_class(DNSClass::IN);
        message.add_query(query);
        let mut record = Record::new();
        record.set_name(name);
        record.set_record_type(RecordType::PTR);
        record.set_dns_class(DNSClass::IN);
        record.set_ttl(20567);
        record.set_rdata(RData::PTR(Name::from_str("localhost").unwrap()));
        message.add_answer(record);
        self.arp_data = message.to_vec().unwrap();
    }

    pub fn ready(&mut self, event: &Event) {
        match event.token() {
            Token(DNS_LOCAL) => {
                self.dispatch_local();
            }
            Token(DNS_TRUSTED) => {
                self.dispatch_trusted();
            }
            Token(DNS_POISONED) => {
                self.dispatch_poisoned();
            }
            _ => unreachable!(),
        }
    }

    fn dispatch_local(&mut self) {
        let now = Instant::now();
        loop {
            match self.listener.recv_from(self.buffer.as_mut_slice()) {
                Ok((length, from)) => {
                    let data = &self.buffer.as_slice()[..length];
                    if let Ok(message) = Message::from_bytes(data) {
                        if message.query_count() == 1 {
                            let query = &message.queries()[0];
                            let name = query.name().to_utf8();
                            if query.query_type() == RecordType::PTR
                                && name == "1.0.0.127.in-addr.arpa."
                            {
                                log::warn!("found ptr query");
                                if let Err(err) =
                                    self.listener.send_to(self.arp_data.as_slice(), from)
                                {
                                    log::error!("send data to {} failed:{}", from, err);
                                }
                                continue;
                            }
                            log::warn!("found query for:{}", name);
                            if let Some(result) = self.store.get(&name) {
                                if !result.response.is_empty()
                                    && (now - result.update_time).as_secs()
                                        < OPTIONS.wintun_args().dns_cache_time
                                {
                                    log::warn!("query found in cache, send now");
                                    if let Err(err) =
                                        self.listener.send_to(result.response.as_slice(), from)
                                    {
                                        log::error!("send response to {} failed:{}", from, err);
                                    }
                                    continue;
                                }
                            }
                            if self.is_blocked(&name) {
                                self.trusted.send(data).unwrap();
                                log::warn!("domain:{} is blocked", name);
                            } else {
                                log::info!("domain:{} is not blocked", name);
                                self.poisoned.send(data).unwrap();
                            }
                            self.add_request(name, from);
                        } else {
                            log::error!(
                                "query count:{} found in message:{:?}",
                                message.query_count(),
                                message
                            );
                        }
                    } else {
                        log::error!("invalid dns message received from {}", from);
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    log::error!("dns listener recv failed:{}", err);
                    break;
                }
            }
        }
    }

    fn dispatch_server(
        recv_socket: &UdpSocket,
        send_socket: &UdpSocket,
        buffer: &mut [u8],
        store: &mut HashMap<String, QueryResult>,
        sender: &Sender<String>,
    ) {
        let now = Instant::now();
        loop {
            match recv_socket.recv_from(buffer) {
                Ok((length, from)) => {
                    let data = &buffer[..length];
                    if let Ok(message) = Message::from_bytes(data) {
                        let name = message.queries()[0].name().to_utf8();
                        if let Some(result) = store.get_mut(&name) {
                            for address in &result.addresses {
                                if let Err(err) = send_socket.send_to(data, *address) {
                                    log::error!("send to {} failed:{}", address, err);
                                } else {
                                    log::warn!("send response to {}", address);
                                }
                            }
                            for record in message.answers() {
                                if let Some(addr) = record.rdata().to_ip_addr() {
                                    if let Err(err) = sender.try_send(addr.to_string()) {
                                        log::error!("send to add route thread failed:{}", err);
                                    } else {
                                        log::warn!("got response {} -> {}", name, addr);
                                    }
                                }
                            }
                            result.update_time = now;
                            result.addresses.clear();
                            result.response.clear();
                            result.response.extend_from_slice(data);
                        }
                    } else {
                        log::error!("invalid dns message received from {}", from);
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    log::error!("dns listener recv failed:{}", err);
                    break;
                }
            }
        }
    }

    fn dispatch_trusted(&mut self) {
        Self::dispatch_server(
            &self.trusted,
            &self.listener,
            self.buffer.as_mut_slice(),
            &mut self.store,
            &self.sender,
        );
    }

    fn dispatch_poisoned(&mut self) {
        Self::dispatch_server(
            &self.poisoned,
            &self.listener,
            self.buffer.as_mut_slice(),
            &mut self.store,
            &self.sender,
        );
    }

    fn is_blocked(&self, name: &String) -> bool {
        self.blocked_domains
            .iter()
            .any(|domain| name.ends_with(domain))
    }
    fn add_request(&mut self, name: String, address: SocketAddr) {
        let result = if let Some(result) = self.store.get_mut(&name) {
            result
        } else {
            self.store.insert(
                name.clone(),
                QueryResult {
                    addresses: vec![],
                    response: vec![],
                    update_time: Instant::now(),
                },
            );
            self.store.get_mut(&name).unwrap()
        };
        result.addresses.push(address);
    }
}