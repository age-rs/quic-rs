#![cfg_attr(all(fuzzing, not(windows)), no_main)]

#[cfg(all(fuzzing, not(windows)))]
use libfuzzer_sys::fuzz_target;

#[cfg(all(fuzzing, not(windows)))]
fuzz_target!(|data: &[u8]| {
    use neqo_common::{Datagram, Encoder, Role};
    use neqo_crypto::Aead;
    use neqo_transport::{packet::MIN_INITIAL_PACKET_SIZE, ConnectionParameters, Version};
    use test_fixture::{
        header_protection::{self, decode_initial_header, initial_aead_and_hp},
        new_client, new_server, now, DEFAULT_ALPN,
    };

    let mut client = new_client(ConnectionParameters::default().mlkem(false));
    let ci = client.process_output(now()).dgram().expect("a datagram");
    let mut server = new_server(DEFAULT_ALPN, ConnectionParameters::default().mlkem(false));
    let si = server.process(Some(ci), now()).dgram().expect("a datagram");

    let Some((header, d_cid, s_cid, payload)) = decode_initial_header(&si, Role::Server) else {
        return;
    };
    let (aead, hp) = initial_aead_and_hp(d_cid, Role::Server);
    let (_, pn) = header_protection::remove(&hp, header, payload);

    let mut payload_enc = Encoder::with_capacity(MIN_INITIAL_PACKET_SIZE);
    payload_enc.encode(data); // Add fuzzed data.

    // Make a new header with a 1 byte packet number length.
    let mut header_enc = Encoder::new();
    header_enc
        .encode_byte(0xc0) // Initial with 1 byte packet number.
        .encode_uint(4, Version::default().wire_version())
        .encode_vec(1, d_cid)
        .encode_vec(1, s_cid)
        .encode_vvec(&[])
        .encode_varint(u64::try_from(payload_enc.len() + Aead::expansion() + 1).unwrap())
        .encode_byte(u8::try_from(pn).unwrap());

    let mut ciphertext = header_enc.as_ref().to_vec();
    ciphertext.resize(header_enc.len() + payload_enc.len() + Aead::expansion(), 0);
    let v = aead
        .encrypt(
            pn,
            header_enc.as_ref(),
            payload_enc.as_ref(),
            &mut ciphertext[header_enc.len()..],
        )
        .unwrap();
    assert_eq!(header_enc.len() + v.len(), ciphertext.len());
    // Pad with zero to get up to MIN_INITIAL_PACKET_SIZE.
    ciphertext.resize(MIN_INITIAL_PACKET_SIZE, 0);

    header_protection::apply(
        &hp,
        &mut ciphertext,
        (header_enc.len() - 1)..header_enc.len(),
    );
    let fuzzed_si = Datagram::new(si.source(), si.destination(), si.tos(), ciphertext);
    let _response = client.process(Some(fuzzed_si), now());
});

#[cfg(any(not(fuzzing), windows))]
fn main() {}
