// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use std::io;
use std::net;
use std::net::UdpSocket;
use std::time::Duration;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use maidsafe_utilities::serialisation::{deserialise, serialise};
use maidsafe_utilities::thread::RaiiThreadJoiner;
use rand;
use sodiumoxide::crypto::sign::PublicKey;

use connection::utp_rendezvous_connect;
use contact_info::ContactInfo;
use event::Event;
use hole_punching::{blocking_udp_punch_hole, external_udp_socket};
use socket_addr::SocketAddr;
use get_if_addrs;
use itertools::Itertools;

const UDP_READ_TIMEOUT_SECS: u64 = 2;

#[derive(RustcEncodable, RustcDecodable)]
pub enum UdpListenerResponse {
    EchoExternalAddr {
        external_addr: SocketAddr,
    },
    Connect {
        connect_on: Vec<SocketAddr>,
        secret: [u8; 4],
        pub_key: PublicKey,
    },
}

#[derive(RustcEncodable, RustcDecodable)]
pub enum UdpListenerRequest {
    EchoExternalAddr,
    Connect {
        secret: [u8; 4],
        pub_key: PublicKey,
    },
}

pub struct UdpListener {
    stop_flag: Arc<AtomicBool>,
    _raii_joiner: RaiiThreadJoiner,
}

impl UdpListener {
    pub fn new(our_contact_info: Arc<Mutex<ContactInfo>>,
               peer_contact_infos: Arc<Mutex<Vec<ContactInfo>>>,
               event_tx: ::CrustEventSender)
               -> io::Result<UdpListener> {
        // TODO: update_contact_info_tx use should be replaced by updating
        // contacts directly by creating a new `BootstrapHandler`
        let udp_socket = try!(UdpSocket::bind("0.0.0.0:0"));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let cloned_stop_flag = stop_flag.clone();

        try!(udp_socket.set_read_timeout(Some(Duration::from_secs(UDP_READ_TIMEOUT_SECS))));
        let port = try!(udp_socket.local_addr()).port();

        // Ask others for our UDP external addresses as they see us. No need to filter out the
        // Local addresses as they will be used by processes in LAN where TCP is disallowed.
        // let echo_external_addr_request =
        //     unwrap_result!(serialise(&UdpListenerMsg::EchoExternalAddr));
        // for peer_contact_info in &*unwrap_result!(peer_contact_infos.lock()) {
        //     for udp_listener in &peer_contact_info.udp_listeners {
        //         let _ = udp_socket.send_to(&echo_external_addr_request, &**udp_listener);
        //     }
        // }

        let if_addrs = try!(get_if_addrs::get_if_addrs())
                           .into_iter()
                           .map(|i| SocketAddr::new(i.addr.ip(), port))
                           .collect_vec();

        unwrap_result!(our_contact_info.lock()).udp_listeners.extend(if_addrs);

        let raii_joiner = RaiiThreadJoiner::new(thread!("UdpListener", move || {
            Self::run(udp_socket, event_tx, peer_contact_infos, cloned_stop_flag);
        }));

        Ok(UdpListener {
            stop_flag: stop_flag,
            _raii_joiner: raii_joiner,
        })
    }

    fn run(udp_socket: UdpSocket,
           event_tx: ::CrustEventSender,
           peer_contact_infos: Arc<Mutex<Vec<ContactInfo>>>,
           stop_flag: Arc<AtomicBool>) {
        let mut read_buf = [0; 1024];

        while !stop_flag.load(Ordering::SeqCst) {
            if let Ok((bytes_read, peer_addr)) = udp_socket.recv_from(&mut read_buf) {
                if let Ok(msg) = deserialise::<UdpListenerRequest>(&read_buf[..bytes_read]) {
                    UdpListener::handle_request(msg,
                                                &udp_socket,
                                                peer_addr,
                                                &event_tx,
                                                &peer_contact_infos);
                } else if let Ok(msg) = deserialise::<UdpListenerResponse>(&read_buf[..bytes_read]) {
                    UdpListener::handle_response(msg,
                                                 &udp_socket,
                                                 peer_addr,
                                                 &event_tx,
                                                 &peer_contact_infos);
                }
            }
        }
    }

    fn handle_request(msg: UdpListenerRequest,
                      udp_socket: &UdpSocket,
                      peer_addr: net::SocketAddr,
                      event_tx: &::CrustEventSender,
                      peer_contact_infos: &Arc<Mutex<Vec<ContactInfo>>>) {
        match msg {
            UdpListenerRequest::EchoExternalAddr => {
                let resp = UdpListenerResponse::EchoExternalAddr {
                    external_addr: SocketAddr(peer_addr.clone()),
                };

                let _ = udp_socket.send_to(&unwrap_result!(serialise(&resp)), peer_addr);
            }
            UdpListenerRequest::Connect { secret, pub_key } => {
                // TODO external_udp_socket() should return the external address
                // of the socket that it freshly spawned or (if it cannot because of say
                // Zero-state etc.) Vector of all interface addresses. This should never be
                // an option because then it is pretty useless.
                let echo_servers = unwrap_result!(peer_contact_infos.lock())
                                       .iter()
                                       .flat_map(|tci| tci.udp_listeners.iter().cloned())
                                       .collect();
                if let Ok(res) = external_udp_socket(rand::random(), echo_servers) {
                    let connect_resp = UdpListenerResponse::Connect {
                        connect_on: vec![res.1],
                        secret: secret,
                        pub_key: pub_key,
                    };

                    if udp_socket.send_to(&unwrap_result!(serialise(&connect_resp)),
                                          peer_addr.clone())
                                 .is_err() {
                        return;
                    }

                    if let (socket, Ok(peer_addr)) =
                           blocking_udp_punch_hole(res.0, Some(secret), SocketAddr(peer_addr)) {
                        let connection = match utp_rendezvous_connect(socket,
                                                                      peer_addr,
                                                                      pub_key.clone(),
                                                                      event_tx.clone()) {
                            Ok(connection) => connection,
                            Err(_) => return,
                        };

                        let event = Event::NewConnection {
                            their_pub_key: pub_key,
                            connection: Ok(connection),
                        };

                        if event_tx.send(event).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }

    fn handle_response(msg: UdpListenerResponse,
                       udp_socket: &UdpSocket,
                       peer_addr: net::SocketAddr,
                       event_tx: &::CrustEventSender,
                       peer_contact_infos: &Arc<Mutex<Vec<ContactInfo>>>) {
        match msg {
            UdpListenerResponse::EchoExternalAddr { external_addr, } => unimplemented!(),
            UdpListenerResponse::Connect { connect_on, secret, pub_key, } => {
                let echo_servers = unwrap_result!(peer_contact_infos.lock())
                                       .iter()
                                       .flat_map(|tci| tci.udp_listeners.iter().cloned())
                                       .collect();
                if let Ok(res) = external_udp_socket(rand::random(), echo_servers) {
                    for peer_addr in connect_on {
                        let s = match res.0.try_clone() {
                            Ok(socket) => socket,
                            Err(_) => return,
                        };
                        let (socket, peer_addr) = match blocking_udp_punch_hole(s,
                                                                                Some(secret),
                                                                                peer_addr) {
                            (socket, Ok(peer_addr)) => (socket, peer_addr),
                            (socket, Err(_)) => return,
                        };

                        let connection = match utp_rendezvous_connect(socket,
                                                                      peer_addr,
                                                                      pub_key.clone(),
                                                                      event_tx.clone()) {
                            Ok(connection) => connection,
                            Err(_) => return,
                        };

                        let event = Event::NewConnection {
                            their_pub_key: pub_key,
                            connection: Ok(connection),
                        };

                        if event_tx.send(event).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for UdpListener {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }
}
