use std::path::PathBuf;
use std::sync::Arc;

use lichii_common::settings::Settings;
use lichii_kernel::ipc::Handle;
use lichii_kernel::memory_objects::{SharedMemory, TransferMemory};
use lichii_kernel::object::KObject;
use lichii_kernel::testing::{IpcClient, Reply, Request};
use lichii_memory::MemoryPermission;

use super::fonts::{FONT_KEY, STAND_INS, decrypt_bfttf};
use super::*;
use crate::testing::{SWITCH1_TITLE, SWITCH2_TITLE, context};

/// A directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("lichii-pl-u-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        for dir in ["nand", "keys", "fonts"] {
            std::fs::create_dir_all(path.join(dir)).unwrap();
        }
        Self(path)
    }

    fn settings(&self) -> Settings {
        let mut settings = Settings::default();
        settings.paths.nand_dir = Some(self.0.join("nand"));
        settings.paths.keys_dir = Some(self.0.join("keys"));
        settings
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn synthetic_font(seed: u8, len: usize) -> Vec<u8> {
    let mut data = vec![0x00, 0x01, 0x00, 0x00];
    data.extend((0..len - 4).map(|i| seed.wrapping_add(i as u8)));
    data
}

fn encrypt_bfttf(plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(0x36f8_1a1eu32.to_be_bytes());
    out.extend((plain.len() as u32 ^ FONT_KEY).to_be_bytes());
    let mut padded = plain.to_vec();
    padded.resize(plain.len().next_multiple_of(4), 0);
    for word in padded.as_chunks::<4>().0 {
        out.extend((u32::from_be_bytes(*word) ^ FONT_KEY).to_be_bytes());
    }
    out
}

fn boot(program_id: u64, dir: &TempDir) -> (Arc<ServiceContext>, IpcClient, Handle) {
    let (context, client) = context(program_id, dir.settings());
    register(&context).unwrap();
    let pl = client.connect("pl:u").expect("pl:u registered");
    (context, client, pl)
}

fn send(client: &IpcClient, session: Handle, request: Request) -> Reply {
    let reply = client.send(session, &request).unwrap();
    assert_eq!(reply.result, ResultCode::SUCCESS, "{request:?}");
    reply
}

fn get<T: bytemuck::Pod>(reply: &Reply, offset: usize) -> T {
    bytemuck::pod_read_unaligned(&reply.data()[offset..offset + size_of::<T>()])
}

fn shared_memory(client: &IpcClient, handle: Handle) -> Arc<SharedMemory> {
    match client.object(handle) {
        Some(KObject::SharedMemory(memory)) => memory,
        _ => panic!("handle {handle:#x} is not shared memory"),
    }
}

fn read(memory: &SharedMemory, offset: u64, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    memory.read(offset, &mut data);
    data
}

/// `(found, offset, size)` of a 2010 or `nx2:pl:font` 0 reply.
fn font_data(client: &IpcClient, session: Handle, request: Request) -> (u32, u32, u32) {
    let reply = send(client, session, request);
    assert_eq!(get::<u32>(&reply, 4), 0);
    (get(&reply, 0), get(&reply, 8), get(&reply, 12))
}

fn priority_list(client: &IpcClient, pl: Handle) -> (u8, u32, [Vec<u32>; 3]) {
    let buffers = [0u8; 24];
    let (codes, offsets, sizes) = (
        client.alloc(&buffers),
        client.alloc(&buffers),
        client.alloc(&buffers),
    );
    let reply = send(
        client,
        pl,
        Request::new(5)
            .arg(0x656e_2d55_5300_0000u64)
            .buffer_b(codes, 24)
            .buffer_b(offsets, 24)
            .buffer_b(sizes, 24),
    );
    let count: u32 = get(&reply, 4);
    let list = [codes, offsets, sizes]
        .map(|at| bytemuck::pod_collect_to_vec(&client.read(at, count as usize * 4)));
    (get(&reply, 0), count, list)
}

fn shared_font_offset(fonts: &[&[u8]], font_type: usize) -> u32 {
    let mut offset = 0u64;
    for font in fonts.iter().take(font_type) {
        offset += 8 + (font.len() as u64).next_multiple_of(4);
    }
    (offset + 8) as u32
}

fn builtin_font_offset(fonts: &[&[u8]], font_type: usize) -> u32 {
    let mut offset = 0u64;
    for font in fonts.iter().take(font_type) {
        offset = (offset + font.len() as u64 + 0xfff) & !0xfff;
    }
    offset as u32
}

fn builtin_memory_size(fonts: &[&[u8]]) -> u32 {
    let mut offset = 0u64;
    for font in fonts {
        offset = (offset + font.len() as u64 + 0xfff) & !0xfff;
    }
    offset.max(0x1000) as u32
}

/// A heap range of the client's process lent as transfer memory; returns its address and handle.
fn transfer_memory(client: &IpcClient, offset: u64, size: u64) -> (u64, Handle) {
    let process = client.process();
    let heap = process.page_table().set_heap_size(2 << 20).unwrap();
    let memory =
        TransferMemory::new(process.clone(), heap + offset, size, MemoryPermission::NONE).unwrap();
    let handle = process
        .handles()
        .write()
        .add(KObject::TransferMemory(Arc::new(memory)))
        .unwrap();
    (heap + offset, handle)
}

#[test]
fn bfttf_round_trips_and_rejects_other_files() {
    let plain = synthetic_font(3, 23);
    assert_eq!(decrypt_bfttf(&encrypt_bfttf(&plain)), Some(plain));
    assert_eq!(decrypt_bfttf(b"\x00\x01\x00\x00 plain font"), None);
    assert_eq!(decrypt_bfttf(b"abc"), None);
}

#[test]
fn serves_font_files_through_the_switch_1_shared_memory() {
    let dir = TempDir::new("switch1");
    let standard = synthetic_font(0x10, 37);
    let extension = synthetic_font(0x80, 40);
    std::fs::write(dir.0.join("fonts/nintendo_udsg-r_std_003.ttf"), &standard).unwrap();
    std::fs::write(
        dir.0.join("fonts/nintendo_ext_003.bfttf"),
        encrypt_bfttf(&extension),
    )
    .unwrap();
    let (_context, client, pl) = boot(SWITCH1_TITLE, &dir);
    let fonts: [&[u8]; 7] = [
        &standard,
        STAND_INS[1],
        STAND_INS[2],
        STAND_INS[3],
        STAND_INS[4],
        &extension,
        STAND_INS[6],
    ];

    let value = |command: u32, font_type: u32| -> u32 {
        get(&send(&client, pl, Request::new(command).arg(font_type)), 0)
    };
    assert_eq!(value(1, 0), LOADED);
    assert_eq!(value(2, 0), standard.len() as u32);
    assert_eq!(value(2, 5), extension.len() as u32);
    assert_eq!(value(2, 1), STAND_INS[1].len() as u32);
    assert_eq!(value(3, 0), shared_font_offset(&fonts, 0));
    assert_eq!(value(3, 5), shared_font_offset(&fonts, 5));

    let handle = send(&client, pl, Request::new(4)).copy_handles[0];
    let memory = shared_memory(&client, handle);
    assert_eq!(memory.size(), SHARED_FONT_MEMORY_SIZE);
    for (font_type, font) in fonts.iter().enumerate() {
        let offset = shared_font_offset(&fonts, font_type);
        let header: Vec<u32> =
            bytemuck::pod_collect_to_vec(&read(&memory, u64::from(offset - 8), 8));
        assert_eq!(header, [0x1802_9a7f, font.len() as u32 ^ FONT_KEY]);
        assert_eq!(&read(&memory, u64::from(offset), font.len()), *font);
    }

    let (loaded, count, [codes, offsets, sizes]) = priority_list(&client, pl);
    assert_eq!((loaded, count), (1, 6));
    assert_eq!(codes, [0, 1, 2, 3, 4, 5]);
    let expected_offsets = (0..6)
        .map(|font_type| shared_font_offset(&fonts, font_type))
        .collect::<Vec<_>>();
    let expected_sizes = fonts[..6]
        .iter()
        .map(|font| font.len() as u32)
        .collect::<Vec<_>>();
    assert_eq!(offsets, expected_offsets);
    assert_eq!(sizes, expected_sizes);
}#[test]
fn serves_plain_font_copies_to_switch_2_titles() {
    let dir = TempDir::new("switch2");
    let standard = synthetic_font(0x10, 37);
    let extension = synthetic_font(0x80, 40);
    std::fs::write(dir.0.join("fonts/nintendo_udsg-r_std_003.ttf"), &standard).unwrap();
    std::fs::write(
        dir.0.join("fonts/nintendo_ext_003.bfttf"),
        encrypt_bfttf(&extension),
    )
    .unwrap();
    let (_context, client, pl) = boot(SWITCH2_TITLE, &dir);
    let fonts: [&[u8]; 7] = [
        &standard,
        STAND_INS[1],
        STAND_INS[2],
        STAND_INS[3],
        STAND_INS[4],
        &extension,
        STAND_INS[6],
    ];
    let type_1_offset = builtin_font_offset(&fonts, 1);
    let extension_offset = builtin_font_offset(&fonts, 5);
    let builtin_size = builtin_memory_size(&fonts);

    assert_eq!(
        font_data(&client, pl, Request::new(2010).arg(0x0105u32)),
        (1, extension_offset, extension.len() as u32)
    );
    assert_eq!(
        font_data(&client, pl, Request::new(2010).arg(0u32)),
        (1, 0, standard.len() as u32)
    );
    assert_eq!(
        font_data(&client, pl, Request::new(2010).arg(1u32)),
        (1, type_1_offset, STAND_INS[1].len() as u32)
    );

    let reply = send(&client, pl, Request::new(2000));
    assert_eq!(get::<u32>(&reply, 0), builtin_size);
    let builtin = shared_memory(&client, reply.copy_handles[0]);
    assert_eq!(builtin.size(), u64::from(builtin_size));
    assert_eq!(read(&builtin, 0, standard.len()), standard);
    assert_eq!(
        read(&builtin, u64::from(type_1_offset), STAND_INS[1].len()),
        STAND_INS[1]
    );
    assert_eq!(
        read(&builtin, u64::from(extension_offset), extension.len()),
        extension
    );

    let (address, transfer) = transfer_memory(&client, 0, u64::from(builtin_size));
    let reply = send(
        &client,
        pl,
        Request::new(2100)
            .arg(0u32)
            .arg(builtin_size)
            .copy_handle(transfer),
    );
    let fonts_session = reply.move_handles[0];
    assert_eq!(
        font_data(&client, fonts_session, Request::new(0).arg(5u32)),
        (1, extension_offset, extension.len() as u32)
    );
    assert_eq!(client.read(address, standard.len()), standard);
    assert_eq!(
        client.read(address + u64::from(type_1_offset), STAND_INS[1].len()),
        STAND_INS[1]
    );
    assert_eq!(
        client.read(address + u64::from(extension_offset), extension.len()),
        extension
    );
    let stub = send(&client, fonts_session, Request::new(9));
    assert_eq!(stub.data(), [0; 128], "nx2:pl:font auto-stubs the rest");

    let (_, small) = transfer_memory(&client, 0x10_0000, 0x1000);
    let reply = client
        .send(
            pl,
            &Request::new(2100)
                .arg(0u32)
                .arg(0x1000u32)
                .copy_handle(small),
        )
        .unwrap();
    assert_eq!(reply.result, lichii_kernel::result::INVALID_SIZE);
}#[test]
fn switch_2_standard_font_gets_the_button_icons() {
    let dir = TempDir::new("icons");
    let standard = font_merge::tests::font(
        1024,
        &[
            font_merge::tests::triangle(10),
            font_merge::tests::triangle(20),
        ],
        &[(0x41, 1)],
        false,
    );
    let extension = font_merge::tests::font(
        1024,
        &[
            font_merge::tests::triangle(10),
            font_merge::tests::triangle(30),
        ],
        &[(0xE0A0, 1)],
        false,
    );
    let merged =
        font_merge::merge_glyphs(&standard, &extension, &font_merge::BUTTON_GLYPHS).unwrap();
    std::fs::write(dir.0.join("fonts/nintendo_udsg-r_std_003.ttf"), &standard).unwrap();
    std::fs::write(dir.0.join("fonts/nintendo_ext_003.ttf"), &extension).unwrap();
    let (_context, client, pl) = boot(SWITCH2_TITLE, &dir);

    let fonts: [&[u8]; 7] = [
        &merged,
        STAND_INS[1],
        STAND_INS[2],
        STAND_INS[3],
        STAND_INS[4],
        &extension,
        STAND_INS[6],
    ];
    let extension_offset = builtin_font_offset(&fonts, 5);
    let type_1_offset = builtin_font_offset(&fonts, 1);
    let builtin_size = builtin_memory_size(&fonts);
    assert_eq!(
        font_data(&client, pl, Request::new(2010).arg(0u32)),
        (1, 0, merged.len() as u32)
    );
    assert_eq!(
        font_data(&client, pl, Request::new(2010).arg(1u32)),
        (1, type_1_offset, STAND_INS[1].len() as u32)
    );
    assert_eq!(
        font_data(&client, pl, Request::new(2010).arg(5u32)),
        (1, extension_offset, extension.len() as u32)
    );
    let reply = send(&client, pl, Request::new(2000));
    assert_eq!(get::<u32>(&reply, 0), builtin_size);
    let builtin = shared_memory(&client, reply.copy_handles[0]);
    assert_eq!(builtin.size(), u64::from(builtin_size));
    assert_eq!(read(&builtin, 0, merged.len()), merged);
    assert_eq!(
        read(&builtin, u64::from(type_1_offset), STAND_INS[1].len()),
        STAND_INS[1]
    );
    assert_eq!(
        read(&builtin, u64::from(extension_offset), extension.len()),
        extension
    );
    let (address, transfer) = transfer_memory(&client, 0, u64::from(builtin_size));
    send(
        &client,
        pl,
        Request::new(2100)
            .arg(0u32)
            .arg(builtin_size)
            .copy_handle(transfer),
    );
    assert_eq!(client.read(address, merged.len()), merged);
    assert_eq!(
        client.read(address + u64::from(type_1_offset), STAND_INS[1].len()),
        STAND_INS[1]
    );
    assert_eq!(
        client.read(address + u64::from(extension_offset), extension.len()),
        extension
    );

    let handle = send(&client, pl, Request::new(4)).copy_handles[0];
    let switch1 = shared_memory(&client, handle);
    assert_eq!(
        read(&switch1, 8, standard.len()),
        standard,
        "Switch 1 titles keep the firmware font"
    );
}#[test]
fn without_fonts_serves_the_open_source_stand_ins() {
    let dir = TempDir::new("stand-ins");
    let (_context, client, pl) = boot(SWITCH2_TITLE, &dir);
    for (font_type, font) in STAND_INS.iter().enumerate() {
        let size: u32 = get(&send(&client, pl, Request::new(2).arg(font_type as u32)), 0);
        assert_eq!(size, font.len() as u32);
    }
    let handle = send(&client, pl, Request::new(4)).copy_handles[0];
    let memory = shared_memory(&client, handle);
    let header: Vec<u32> = bytemuck::pod_collect_to_vec(&read(&memory, 0, 8));
    assert_eq!(header, [0x1802_9a7f, STAND_INS[0].len() as u32 ^ FONT_KEY]);
    assert_eq!(read(&memory, 8, STAND_INS[0].len()), STAND_INS[0]);

    let (loaded, count, [codes, ..]) = priority_list(&client, pl);
    assert_eq!((loaded, count), (1, 6));
    assert_eq!(codes, [0, 1, 2, 3, 4, 5]);
    for font_type in 0..7u32 {
        let (found, _, size) = font_data(&client, pl, Request::new(2010).arg(font_type));
        assert_eq!(found, 1, "font type {font_type} is found");
        assert_ne!(size, 0);
    }
}
