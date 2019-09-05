use crate::GameVersion;
use crate::byteio::{ReadBytes, WriteBytes};

use flate2::read::ZlibDecoder;

use std::{
    convert::TryInto,
    error::Error,
    fmt::{self, Display},
    iter::once,
    io::{self, Read, Seek, SeekFrom},
};

macro_rules! log {
    ($logger: expr, $x: expr) => {
        if let Some(logger) = &$logger {
            logger($x.into());
        }
    };
    ($logger: expr, $format: expr, $($x: expr),*) => {
        if let Some(logger) = &$logger {
            logger(&format!(
                $format,
                $($x),*
            ));
        }
    };
}

#[derive(Debug)]
pub enum ReaderError {
    IO(io::Error),
    PartialUPXPacking,
    UnknownFormat,
}
impl Error for ReaderError {}
impl Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", match self {
            ReaderError::IO(err) => format!("io error: {}", err),
            ReaderError::PartialUPXPacking => format!("looks upx protected, can't locate headers"),
            ReaderError::UnknownFormat => format!("unknown format, could not identify file")
        })
    }
}
impl From<io::Error> for ReaderError {
    fn from(err: io::Error) -> Self {
        ReaderError::IO(err)
    }
}


const GM80_HEADER_START_POS: u64 = 0x144AC0;

/// Identifies the game version and start of gamedata header, given a data cursor. Also removes any version-specific encryptions.
pub fn find_gamedata<F>(exe: &mut io::Cursor<&mut [u8]>, logger: Option<F>) -> Result<GameVersion, ReaderError>
where
    F: Copy + Fn(&str)
{
    // Check for UPX-signed PE header
    exe.set_position(0x170);
    // Check for "UPX0" header
    if &exe.read_u32_le()?.to_le_bytes() == b"UPX0" {
        if logger.is_some() {
            log!(logger, "Found UPX0 header at {}", exe.position() - 4);
        }

        exe.seek(SeekFrom::Current(36))?;
        // Check for "UPX1" header
        if &exe.read_u32_le()?.to_le_bytes() == b"UPX1" {
            exe.seek(SeekFrom::Current(76))?;

            // Read the UPX version which is a null-terminated string.
            if logger.is_some() {
                let mut upx_ver = String::with_capacity(4); // Usually "3.03"
                while let Ok(ch) = exe.read_u8() {
                    if ch != 0 {
                        upx_ver.push(ch as char);
                    } else {
                        break;
                    }
                }
                log!(logger, "Found UPX version {}", upx_ver);
            } else {
                while exe.read_u8()? != 0 {}
            }

            if &exe.read_u32_le()?.to_le_bytes() == b"UPX!" {
                //"UPX!"
                exe.seek(SeekFrom::Current(28))?;

                let mut unpacked = unpack_upx(exe, logger)?;
                log!(logger, "Successfully unpacked UPX - output is {} bytes", unpacked.len());
                let mut unpacked = io::Cursor::new(&mut *unpacked);

                // UPX unpacked, now check if this is a supported data format
                if let Some((exe_load_offset, header_start, xor_mask, add_mask, sub_mask)) =
                    check_antidec(&mut unpacked)?
                {
                    if logger.is_some() {
                        log!(
                            logger, 
                            concat!(
                                "Found antidec2 loading sequence, decrypting with the following values:\n",
                                "exe_load_offset:0x{:X} header_start:0x{:X} xor_mask:0x{:X} add_mask:0x{:X} sub_mask:0x{:X}"
                            ),
                            exe_load_offset, header_start, xor_mask, add_mask, sub_mask
                        );
                    }
                    decrypt_antidec(exe, exe_load_offset, header_start, xor_mask, add_mask, sub_mask)?;

                    // 8.0-specific header, but no point strict-checking it because antidec puts random garbage there.
                    exe.seek(SeekFrom::Current(12))?;
                    return Ok(GameVersion::GameMaker8_0);
                } else {
                    return Err(ReaderError::UnknownFormat);
                }
            } else {
                return Err(ReaderError::PartialUPXPacking);
            }
        }
    }

    // Check for antidec2 protection in the base exe (so without UPX on top of it)
    if let Some((exe_load_offset, header_start, xor_mask, add_mask, sub_mask)) = check_antidec(exe)? {
        if logger.is_some() {
            log!(
                logger, 
                concat!(
                    "Found antidec2 loading sequence [no UPX], decrypting with the following values:\n",
                    "exe_load_offset:0x{:X} header_start:0x{:X} xor_mask:0x{:X} add_mask:0x{:X} sub_mask:0x{:X}",
                ),
                exe_load_offset, header_start, xor_mask, add_mask, sub_mask
            );
        }
        decrypt_antidec(exe, exe_load_offset, header_start, xor_mask, add_mask, sub_mask)?;

        // 8.0-specific header, but no point strict-checking it because antidec puts random garbage there.
        exe.seek(SeekFrom::Current(12))?;
        return Ok(GameVersion::GameMaker8_0);
    }

    // Standard formats
    if check_gm80(exe, logger)? {
        Ok(GameVersion::GameMaker8_0)
    } else if check_gm81(exe, logger)? {
        Ok(GameVersion::GameMaker8_1)
    } else {
        Err(ReaderError::UnknownFormat)
    }
}

/// Check if this is a standard gm8.0 game by looking for the loading sequence
/// If so, sets the cursor to the start of the gamedata.
fn check_gm80<F>(exe: &mut io::Cursor<&mut [u8]>, logger: Option<F>) -> Result<bool, ReaderError>
where
    F: Fn(&str),
{
    log!(logger, "Checking for standard GM8.0 format...");

    // Verify size is large enough to do the following checks - otherwise it can't be this format
    if exe.get_ref().len() < (GM80_HEADER_START_POS as usize) + 4 {
        log!(logger, "File too short for this format (0x{:X} bytes)", exe.get_ref().len());
        return Ok(false);
    }

    // Check for the standard 8.0 loading sequence
    exe.set_position(0x000A49BE);
    let mut buf = [0u8; 8];
    exe.read_exact(&mut buf)?;
    if buf == [0x8B, 0x45, 0xF4, 0xE8, 0x2A, 0xBD, 0xFD, 0xFF] {
        // Looks like GM8.0 so let's parse the rest of loading sequence.
        // If the next byte isn't a CMP, the GM8.0 magic check has been patched out.
        let gm80_magic: Option<u32> = match exe.read_u8()? {
            0x3D => {
                let magic = exe.read_u32_le()?;
                let mut buf = [0u8; 6];
                exe.read_exact(&mut buf)?;
                if buf == [0x0F, 0x85, 0x18, 0x01, 0x00, 0x00] {
                    log!(logger, "GM8.0 magic check looks intact - value is {}", magic);
                    Some(magic)
                }
                else {
                    log!(logger, "GM8.0 magic check's JNZ is patched out");
                    None
                }
            },
            0x90 => {
                exe.seek(SeekFrom::Current(4))?;
                log!(logger, "GM8.0 magic check is patched out with NOP");
                None
            },
            i => {
                log!(logger, "Unknown instruction in place of magic CMP: {}", i);
                return Ok(false);
            }
        };

        // There should be a CMP for the next dword, it's usually a version header (0x320)
        let gm80_header_ver: Option<u32> = {
            exe.set_position(0x000A49E2);
            let mut buf = [0u8; 7];
            exe.read_exact(&mut buf)?;
            if buf == [0x8B, 0xC6, 0xE8, 0x07, 0xBD, 0xFD, 0xFF] {
                match exe.read_u8()? {
                    0x3D => {
                        let magic = exe.read_u32_le()?;
                        let mut buf = [0u8; 6];
                        exe.read_exact(&mut buf)?;
                        if buf == [0x0F, 0x85, 0xF5, 0x00, 0x00, 0x00] {
                            log!(logger, "GM8.0 header version check looks intact - value is {}", magic);
                            Some(magic)
                        }
                        else {
                            println!("GM8.0 header version check's JNZ is patched out");
                            None
                        }
                    },
                    0x90 => {
                        exe.seek(SeekFrom::Current(4))?;
                        log!(logger, "GM8.0 header version check is patched out with NOP");
                        None
                    },
                    i => {
                        log!(logger, "Unknown instruction in place of magic CMP: {}", i);
                        return Ok(false);
                    }
                }
            }
            else {
                log!(logger, "GM8.0 header version check appears patched out");
                None
            }
        };

        // Read header start pos
        exe.set_position(GM80_HEADER_START_POS);
        let header_start = exe.read_u32_le()?;
        log!(logger, "Reading header from 0x{:X}", header_start);
        exe.set_position(header_start as u64);

        // Check the header magic numbers are what we read them as
        match gm80_magic {
            Some(n) => {
                let header1 = exe.read_u32_le()?;
                if header1 != n {
                    log!(logger, "Failed to read GM8.0 header: expected {} at {}, got {}", n, header_start, header1);
                    return Ok(false);
                }
            },
            None => {
                exe.seek(SeekFrom::Current(4))?;
            }
        }
        match gm80_header_ver {
            Some(n) => {
                let header2 = exe.read_u32_le()?;
                if header2 != n {
                    log!(logger, "Failed to read GM8.0 header: expected version {}, got {}", n, header2);
                    return Ok(false);
                }
            },
            None => {
                exe.seek(SeekFrom::Current(4))?;
            }
        }

        exe.seek(SeekFrom::Current(8))?;
        Ok(true)
    }
    else {
        Ok(false)
    }
}

/// Check if this is a standard gm8.1 game by looking for the loading sequence
/// If so, removes gm81 encryption and sets the cursor to the start of the gamedata.
fn check_gm81<F>(exe: &mut io::Cursor<&mut [u8]>, logger: Option<F>) -> Result<bool, ReaderError>
where
    F: Fn(&str),
{
    log!(logger, "Checking for standard GM8.1 format");

    // Verify size is large enough to do the following checks - otherwise it can't be this format
    if exe.get_ref().len() < 0x226D8A {
        log!(logger, "File too short for this format (0x{:X} bytes)", exe.get_ref().len());
        return Ok(false);
    }

    // Check for the standard 8.1 loading sequence
    exe.set_position(0x00226CF3);
    let mut buf = [0u8; 8];
    exe.read_exact(&mut buf)?;
    if buf == [0xE8, 0x80, 0xF2, 0xDD, 0xFF, 0xC7, 0x45, 0xF0] {
        // Looks like GM8.1 so let's parse the rest of loading sequence.
        // Next dword is the point where we start reading the header
        let header_start = exe.read_u32_le()?;

        // Next we'll read the magic value
        exe.seek(SeekFrom::Current(125))?;
        let mut buf = [0u8; 3];
        exe.read_exact(&mut buf)?;
        let gm81_magic: Option<u32> = match buf {
            [0x81, 0x7D, 0xEC] => {
                let magic = exe.read_u32_le()?;
                if exe.read_u8()? == 0x74 {
                    log!(logger, "GM8.1 magic check looks intact - value is 0x{:X}", magic);
                    Some(magic)
                }
                else {
                    println!("GM8.1 magic check's JE is patched out");
                    None
                }
            }
            b => {
                println!("GM8.1 magic check's CMP is patched out ({:?})", b);
                None
            }
        };

        // Search for header
        exe.set_position(header_start as u64);
        match gm81_magic {
            Some(n) => {
                log!(logger, "Searching for GM8.1 magic number {} from position {}", n, header_start);
                let found_header = {
                    let mut i = header_start as u64;
                    loop {
                        exe.set_position(i);
                        let val = (exe.read_u32_le()? & 0xFF00FF00) + (exe.read_u32_le()? & 0x00FF00FF);
                        if val == n {
                            break true;
                        }
                        i += 1;
                        if ((i + 8) as usize) >= exe.get_ref().len() {
                            break false;
                        }
                    }
                };
                if !found_header {
                    log!(logger, "Didn't find GM81 magic value (0x{:X}) before EOF, so giving up", n);
                    return Ok(false);
                }
            },
            None => {
                exe.seek(SeekFrom::Current(8))?;
            }
        }

        decrypt_gm81(exe, logger)?;
        exe.seek(SeekFrom::Current(20))?;
        Ok(true)
    }
    else {
        Ok(false)
    }
}

/// Removes GM8.1 encryption in-place.
fn decrypt_gm81<F>(data: &mut io::Cursor<&mut [u8]>, logger: Option<F>) -> io::Result<()>
where
    F: Fn(&str),
{
    // YYG's crc32 implementation
    let crc_32 = |hash_key: &Vec<u8>, crc_table: &[u32; 256]| -> u32 {
        let mut result: u32 = 0xFFFFFFFF;
        for c in hash_key.iter() {
            result = (result >> 8) ^ crc_table[((result & 0xFF) as u8 ^ c) as usize];
        }
        result
    };
    let crc_32_reflect = |mut value: u32, c: i8| -> u32 {
        let mut rvalue: u32 = 0;
        for i in 1..=c {
            if value & 1 != 0 {
                rvalue |= 1 << (c - i);
            }
            value >>= 1;
        }
        rvalue
    };

    let hash_key = format!("_MJD{}#RWK", data.read_u32_le()?);
    let hash_key_utf16: Vec<u8> = hash_key.bytes().flat_map(|c| once(c).chain(once(0))).collect();

    // generate crc table
    let mut crc_table = [0u32; 256];
    let crc_polynomial: u32 = 0x04C11DB7;
    for i in 0..256 {
        crc_table[i] = crc_32_reflect(i as u32, 8) << 24;
        for _ in 0..8 {
            crc_table[i] = (crc_table[i] << 1)
                ^ (if crc_table[i] & (1 << 31) != 0 {
                    crc_polynomial
                } else {
                    0
                });
        }
        crc_table[i] = crc_32_reflect(crc_table[i], 32);
    }

    // get our two seeds for generating xor masks
    let mut seed1 = data.read_u32_le()?;
    let mut seed2 = crc_32(&hash_key_utf16, &crc_table);

    log!(
        logger,
            "Decrypting GM8.1 protection (hashkey: {}, seed1: {}, seed2: {})",
            hash_key, seed1, seed2
        );

    // skip to where gm81 encryption starts
    let old_position = data.position();
    data.seek(SeekFrom::Current(((seed2 & 0xFF) + 0xA) as i64))?;

    // Decrypt stream from here
    while let Ok(dword) = data.read_u32_le() {
        data.set_position(data.position() - 4);
        seed1 = (0xFFFF & seed1) * 0x9069 + (seed1 >> 16);
        seed2 = (0xFFFF & seed2) * 0x4650 + (seed2 >> 16);
        let xor_mask = (seed1 << 16) + (seed2 & 0xFFFF);
        data.write_u32_le(xor_mask ^ dword)?;
    }

    data.set_position(old_position);
    Ok(())
}

/// Helper function for inflating zlib data. A preceding u32 indicating size is assumed.
fn _inflate<I>(data: &I) -> Result<Vec<u8>, ReaderError>
where
    I: AsRef<[u8]> + ?Sized,
{
    let slice = data.as_ref();
    let mut decoder = ZlibDecoder::new(slice);
    let mut buf: Vec<u8> = Vec::with_capacity(slice.len());
    decoder.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Helper function for checking whether a data stream looks like an antidec2-protected exe.
/// If so, returns the relevant vars to decrypt the data stream (exe_load_offset, header_start, xor_mask, add_mask, sub_mask).
fn check_antidec(exe: &mut io::Cursor<&mut [u8]>) -> Result<Option<(u32, u32, u32, u32, u32)>, ReaderError> {
    // Verify size is large enough to do the following checks - otherwise it can't be antidec
    if exe.get_ref().len() < (GM80_HEADER_START_POS as usize) + 4 {
        return Ok(None);
    }

    // Check for the loading sequence
    exe.set_position(0x00032337);
    let mut buf = [0u8; 8];
    exe.read_exact(&mut buf)?;
    if buf == [0xE2, 0xF7, 0xC7, 0x05, 0x2E, 0x2F, 0x43, 0x00] {
        // Looks like antidec's loading sequence, so let's extract values from it
        // First, the xor byte that's used to decrypt the decryption code (yes you read that right)
        exe.seek(SeekFrom::Current(-9))?;
        let byte_xor_mask = exe.read_u8()?;
        // Convert it into a u32 mask so we can apply it easily to dwords
        let dword_xor_mask = u32::from_ne_bytes([byte_xor_mask, byte_xor_mask, byte_xor_mask, byte_xor_mask]);
        // Next, the file offset for loading gamedata bytes
        exe.set_position(0x000322A9);
        let exe_load_offset = exe.read_u32_le()? ^ dword_xor_mask;
        // Now the header_start from later in the file
        exe.set_position(GM80_HEADER_START_POS);
        let header_start = exe.read_u32_le()?;
        // xor mask
        exe.set_position(0x000322D3);
        let xor_mask = exe.read_u32_le()? ^ dword_xor_mask;
        // add mask
        exe.set_position(0x000322D8);
        let add_mask = exe.read_u32_le()? ^ dword_xor_mask;
        // sub mask
        exe.set_position(0x000322E4);
        let sub_mask = exe.read_u32_le()? ^ dword_xor_mask;
        Ok(Some((exe_load_offset, header_start, xor_mask, add_mask, sub_mask)))
    } else {
        Ok(None)
    }
}

/// Removes antidec2 encryption from gamedata, given the IVs required to do so.
/// Also sets the cursor to the start of the gamedata.
fn decrypt_antidec(
    data: &mut io::Cursor<&mut [u8]>,
    exe_load_offset: u32,
    header_start: u32,
    mut xor_mask: u32,
    mut add_mask: u32,
    sub_mask: u32,
) -> Result<(), ReaderError> {
    let game_data = data.get_mut().get_mut(exe_load_offset as usize..).unwrap(); // <- TODO
    for chunk in game_data.rchunks_exact_mut(4) {
        // TODO: fix this when const generics start existing
        let chunk: &mut [u8; 4] = chunk.try_into()
            .unwrap_or_else(|_| unsafe { std::hint::unreachable_unchecked() });
        let mut value = u32::from_le_bytes(*chunk);

        // apply masks, bswap
        value ^= xor_mask;
        value = value.wrapping_add(add_mask);
        value = value.swap_bytes();

        // cycle masks
        xor_mask = xor_mask.wrapping_sub(sub_mask);
        add_mask = add_mask.swap_bytes().wrapping_add(1);

        // write decrypted value
        *chunk = value.to_le_bytes();
    }

    data.set_position((exe_load_offset + header_start + 4) as u64);
    Ok(())
}

/// Unpack the bytecode of a UPX-protected exe into a separate buffer
fn unpack_upx<F>(data: &mut io::Cursor<&mut [u8]>, logger: Option<F>) -> Result<Vec<u8>, ReaderError>
where
    F: Fn(&str),
{
    // Locate PE header and read code entry point
    // Note: I am not sure how to read the full length of the data section, but UPX's entry point is always after the
    // area it extracts to, so it should always suffice as an output size. We could also read the ImageBase from here, but
    // since BOTH the code section and entry point are already relative to ImageBase, there's no need.
    data.set_position(0x3C);
    let pe_header = data.read_u8()?;
    data.set_position(pe_header as u64 + 40);
    let entry_point = data.read_u32_le()?;
    data.seek(SeekFrom::Current(361))?;

    let mut output: Vec<u8> = vec![0u8; entry_point as usize];
    let mut u_var2: u8;
    let mut i_var5: i32;
    let mut u_var6: u32;
    let mut pu_var8: u32;
    let mut u_var9: u32;
    let mut u_var10: u32;
    let mut u_var12: u32 = 0xFFFFFFFF;
    let mut pu_var14: u32 = 0x400; // Cursor for output vec
    let mut did_wrap17: bool;
    let mut did_wrap18: bool;

    u_var9 = data.read_u32_le()?;

    log!(logger, "UPX entry point: 0x{:X}; unpacker IV: {}", entry_point, u_var9);

    did_wrap18 = u_var9 >= 0x80000000;
    u_var9 = u_var9.wrapping_mul(2).wrapping_add(1);

    let mut pull_new: bool = false;
    loop {
        // LAB_0
        if pull_new {
            u_var9 = data.read_u32_le()?;
            did_wrap18 = u_var9 >= 0x80000000;
            u_var9 = u_var9.wrapping_mul(2).wrapping_add(1);
        }
        // LAB_2
        if did_wrap18 {
            loop {
                let u_var2: u8 = data.read_u8()?;
                output[pu_var14 as usize] = u_var2; // TODO: this is bounds checked, very slow
                pu_var14 += 1;
                did_wrap18 = u_var9 >= 0x80000000;
                u_var9 = u_var9.wrapping_mul(2);
                if (u_var9 == 0) || (!did_wrap18) {
                    break;
                }
            }
            if u_var9 == 0 {
                pull_new = true;
                continue; // goto LAB_0
            }
        }

        i_var5 = 1;
        loop {
            did_wrap17 = u_var9 >= 0x80000000;
            u_var10 = u_var9.wrapping_mul(2);
            if u_var10 == 0 {
                u_var9 = data.read_u32_le()?;
                did_wrap17 = u_var9 >= 0x80000000;
                u_var10 = u_var9.wrapping_mul(2).wrapping_add(1);
            }
            u_var6 = (2 * (i_var5 as u32)) + if did_wrap17 { 1 } else { 0 };
            u_var9 = u_var10.wrapping_mul(2);
            if u_var10 >= 0x80000000 {
                // if (CARRY4(uVar10,uVar10)) {
                if u_var9 != 0 {
                    break;
                }
                u_var10 = data.read_u32_le()?;
                u_var9 = u_var10.wrapping_mul(2).wrapping_add(1);
                if u_var10 >= 0x80000000 {
                    break;
                }
            }
            did_wrap17 = u_var9 >= 0x80000000;
            u_var9 = u_var9.wrapping_mul(2);
            if u_var9 == 0 {
                u_var9 = data.read_u32_le()?;
                did_wrap17 = u_var9 >= 0x80000000;
                u_var9 = u_var9.wrapping_mul(2).wrapping_add(1);
            }
            i_var5 = ((u_var6 - 1) * 2 + if did_wrap17 { 1 } else { 0 }) as i32;
        }

        i_var5 = 0;
        if u_var6 < 3 {
            did_wrap17 = u_var9 >= 0x80000000;
            u_var9 = u_var9.wrapping_mul(2);
            if u_var9 == 0 {
                u_var9 = data.read_u32_le()?;
                did_wrap17 = u_var9 >= 0x80000000;
                u_var9 = u_var9.wrapping_mul(2).wrapping_add(1);
            }
        } else {
            u_var2 = data.read_u8()?;
            // This is weird because it copies a byte into AL then xors all of EAX, which has a dead value left in its other bytes.
            u_var12 = ((((u_var6 - 3) << 8) & 0xFFFFFF00) + (u_var2 as u32 & 0xFF)) ^ 0xFFFFFFFF;
            if u_var12 == 0 {
                break; // This is the only exit point
            }
            did_wrap17 = (u_var12 & 1) != 0;
            u_var12 = ((u_var12 as i32) >> 1) as u32;
        }

        let mut b: bool = true;
        if !did_wrap17 {
            i_var5 += 1;
            did_wrap17 = u_var9 >= 0x80000000;
            u_var9 = u_var9.wrapping_mul(2);
            if u_var9 == 0 {
                u_var9 = data.read_u32_le()?;
                did_wrap17 = u_var9 >= 0x80000000;
                u_var9 = u_var9.wrapping_mul(2).wrapping_add(1);
            }
            if !did_wrap17 {
                loop {
                    loop {
                        did_wrap17 = u_var9 >= 0x80000000;
                        u_var10 = u_var9.wrapping_mul(2);
                        if u_var10 == 0 {
                            u_var9 = data.read_u32_le()?;
                            did_wrap17 = u_var9 >= 0x80000000;
                            u_var10 = u_var9.wrapping_mul(2).wrapping_add(1);
                        }
                        i_var5 = (i_var5 * 2) + if did_wrap17 { 1 } else { 0 };
                        u_var9 = u_var10.wrapping_mul(2);
                        if u_var10 >= 0x80000000 {
                            break;
                        }
                    }

                    if u_var9 != 0 {
                        break;
                    }
                    u_var10 = data.read_u32_le()?;
                    u_var9 = u_var10.wrapping_mul(2).wrapping_add(1);
                    if u_var10 >= 0x80000000 {
                        break;
                    }
                }
                i_var5 += 2;
                b = false;
            }
        }

        if b {
            did_wrap17 = u_var9 >= 0x80000000;
            u_var9 = u_var9.wrapping_mul(2);
            if u_var9 == 0 {
                u_var9 = data.read_u32_le()?;
                did_wrap17 = u_var9 >= 0x80000000;
                u_var9 = u_var9.wrapping_mul(2).wrapping_add(1);
            }
            i_var5 = (i_var5 * 2) + if did_wrap17 { 1 } else { 0 };
        }

        u_var10 = (i_var5 as u32) + 2 + if u_var12 < 0xfffffb00 { 1 } else { 0 }; // No idea, just going with it.

        pu_var8 = pu_var14.wrapping_add(u_var12);
        if u_var12 < 0xfffffffd {
            loop {
                // uVar4 = *puVar8;
                let uv1 = output[pu_var8 as usize];
                let uv2 = output[(pu_var8 + 1) as usize];
                let uv3 = output[(pu_var8 + 2) as usize];
                let uv4 = output[(pu_var8 + 3) as usize];
                // puVar8 = puVar8 + 1; (ADD EDX,0x4)
                pu_var8 += 4;
                // *puVar14 = uVar4;
                output[pu_var14 as usize] = uv1;
                output[(pu_var14 + 1) as usize] = uv2;
                output[(pu_var14 + 2) as usize] = uv3;
                output[(pu_var14 + 3) as usize] = uv4;
                // puVar14 = puVar14 + 1; (ADD EDI,0x4)
                pu_var14 += 4;

                did_wrap17 = 3 < u_var10;
                u_var10 = u_var10.wrapping_sub(4);
                if (!did_wrap17) || (u_var10 == 0) {
                    break;
                }
            }
            pu_var14 = pu_var14.wrapping_add(u_var10);
        } else {
            loop {
                u_var2 = output[pu_var8 as usize];
                pu_var8 += 1;
                output[pu_var14 as usize] = u_var2;
                pu_var14 += 1;
                u_var10 = u_var10.wrapping_sub(1);

                if u_var10 == 0 {
                    break;
                }
            }
        }

        did_wrap18 = u_var9 >= 0x80000000;
        u_var9 = u_var9.wrapping_mul(2);
        pull_new = u_var9 == 0;
    }

    Ok(output)
}