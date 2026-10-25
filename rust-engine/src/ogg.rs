//! Ogg pages (RFC 3533), the container of the app's recordings: reading
//! pages back and writing packets out a page at a time.

use std::io::{self, Read, Write};

const CAPTURE_PATTERN: &[u8; 4] = b"OggS";
const HEADER_BYTES: usize = 27;
/// Lacing values in one page; each one covers up to 255 bytes of a packet.
const MAX_SEGMENTS: usize = 255;
/// Byte offset of the checksum within the page header.
const CRC_OFFSET: usize = 22;

/// The page starts with the rest of a packet from the previous page.
pub(crate) const FLAG_CONTINUED: u8 = 0x01;
/// First page of the stream.
pub(crate) const FLAG_BEGIN: u8 = 0x02;
/// Last page of the stream.
pub(crate) const FLAG_END: u8 = 0x04;
/// Granule position of a page on which no packet ends.
pub(crate) const NO_GRANULE: i64 = -1;

/// CRC-32 with polynomial 0x04c11db7, unreflected, no final XOR (Ogg's).
const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04c1_1db7
            } else {
                r << 1
            };
            bit += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}

fn crc(bytes: &[u8]) -> u32 {
    crc_update(0, bytes)
}

fn crc_update(crc: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(crc, |crc, &b| {
        (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ b) as usize]
    })
}

/// One page as read.
pub(crate) struct Page {
    pub flags: u8,
    pub granule: i64,
    pub serial: u32,
    /// Length of each segment of the body; one under 255 ends a packet.
    lacing: Vec<u8>,
    body: Vec<u8>,
    /// The whole page's length in the file.
    pub len: u64,
}

impl Page {
    /// The body split at packet boundaries: (bytes, whether a packet ends
    /// there). The first piece continues the previous page's last packet if
    /// the page has [`FLAG_CONTINUED`]; the last may go on in the next page.
    pub(crate) fn pieces(&self) -> Vec<(&[u8], bool)> {
        let mut pieces = Vec::new();
        let (mut start, mut end) = (0, 0);
        for (i, &len) in self.lacing.iter().enumerate() {
            end += usize::from(len);
            let last = i + 1 == self.lacing.len();
            if len < 255 || last {
                pieces.push((&self.body[start..end], len < 255));
                start = end;
            }
        }
        pieces
    }
}

/// The page at the reader's position. `Ok(None)` at the end of the file, and
/// also for a page cut short (a recording's tail after a crash); `InvalidData`
/// if it isn't an Ogg page or fails its checksum.
pub(crate) fn read_page(r: &mut impl Read) -> io::Result<Option<Page>> {
    let mut header = [0u8; HEADER_BYTES];
    if !read_full(r, &mut header)? {
        return Ok(None);
    }
    if &header[0..4] != CAPTURE_PATTERN || header[4] != 0 {
        return Err(invalid("not an Ogg page"));
    }
    let mut lacing = vec![0u8; usize::from(header[26])];
    if !read_full(r, &mut lacing)? {
        return Ok(None);
    }
    let mut body = vec![0u8; lacing.iter().map(|&l| usize::from(l)).sum()];
    if !read_full(r, &mut body)? {
        return Ok(None);
    }
    let stored = u32::from_le_bytes([header[22], header[23], header[24], header[25]]);
    header[CRC_OFFSET..CRC_OFFSET + 4].fill(0);
    let sum = crc_update(crc_update(crc(&header), &lacing), &body);
    if sum != stored {
        return Err(invalid("Ogg page checksum mismatch"));
    }
    let mut granule = [0u8; 8];
    granule.copy_from_slice(&header[6..14]);
    Ok(Some(Page {
        flags: header[5],
        granule: i64::from_le_bytes(granule),
        serial: u32::from_le_bytes([header[14], header[15], header[16], header[17]]),
        len: (HEADER_BYTES + lacing.len() + body.len()) as u64,
        lacing,
        body,
    }))
}

/// Fills `buf`; false if the reader ends first.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Collects whole packets into pages of one logical stream and writes each
/// page with a single write once it is complete. Packets never span pages.
pub(crate) struct PageWriter<W: Write> {
    out: W,
    serial: u32,
    sequence: u32,
    lacing: Vec<u8>,
    body: Vec<u8>,
    packets: usize,
    /// Granule position after the last packet added.
    granule: i64,
}

impl<W: Write> PageWriter<W> {
    pub(crate) fn new(out: W, serial: u32) -> Self {
        Self {
            out,
            serial,
            sequence: 0,
            lacing: Vec::new(),
            body: Vec::new(),
            packets: 0,
            granule: 0,
        }
    }

    /// Packets on the page being collected.
    pub(crate) fn pending_packets(&self) -> usize {
        self.packets
    }

    /// Adds a packet ending at `granule` to the page being collected, first
    /// writing that page out if the packet wouldn't fit in it.
    pub(crate) fn add(&mut self, packet: &[u8], granule: i64) -> io::Result<()> {
        let segments = packet.len() / 255 + 1;
        if segments > MAX_SEGMENTS {
            return Err(invalid("packet too large for one Ogg page"));
        }
        if self.lacing.len() + segments > MAX_SEGMENTS {
            self.write_page(0)?;
        }
        self.lacing.extend(std::iter::repeat_n(255, segments - 1));
        self.lacing.push((packet.len() % 255) as u8);
        self.body.extend_from_slice(packet);
        self.packets += 1;
        self.granule = granule;
        Ok(())
    }

    /// Writes the collected packets as one page with `flags`. With nothing
    /// collected, writes a page only if `flags` has [`FLAG_END`].
    pub(crate) fn write_page(&mut self, flags: u8) -> io::Result<()> {
        if self.packets == 0 && flags & FLAG_END == 0 {
            return Ok(());
        }
        let granule = if self.packets == 0 {
            NO_GRANULE
        } else {
            self.granule
        };
        let mut page = Vec::with_capacity(HEADER_BYTES + self.lacing.len() + self.body.len());
        page.extend_from_slice(CAPTURE_PATTERN);
        page.push(0); // version
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        page.extend_from_slice(&[0; 4]); // checksum, filled in below
        page.push(self.lacing.len() as u8);
        page.extend_from_slice(&self.lacing);
        page.extend_from_slice(&self.body);
        let sum = crc(&page);
        page[CRC_OFFSET..CRC_OFFSET + 4].copy_from_slice(&sum.to_le_bytes());
        self.out.write_all(&page)?;
        self.sequence = self.sequence.wrapping_add(1);
        self.lacing.clear();
        self.body.clear();
        self.packets = 0;
        Ok(())
    }

    pub(crate) fn into_inner(self) -> W {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_all(bytes: &[u8]) -> Vec<Page> {
        let mut r = Cursor::new(bytes);
        std::iter::from_fn(|| read_page(&mut r).unwrap()).collect()
    }

    #[test]
    fn crc_matches_the_ogg_reference() {
        // CRC-32/POSIX's check value (0x765e7680) without its final XOR.
        assert_eq!(crc(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn packets_round_trip_through_pages() {
        let mut w = PageWriter::new(Vec::new(), 7);
        w.add(b"head", 0).unwrap();
        w.write_page(FLAG_BEGIN).unwrap();
        let long = vec![9u8; 600];
        w.add(&long, 960).unwrap();
        w.add(b"", 1920).unwrap();
        w.add(b"x", 2880).unwrap();
        w.write_page(FLAG_END).unwrap();
        let pages = read_all(&w.into_inner());

        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].flags, FLAG_BEGIN);
        assert_eq!(pages[0].serial, 7);
        assert_eq!(pages[0].pieces(), [(&b"head"[..], true)]);
        assert_eq!(pages[1].flags, FLAG_END);
        assert_eq!(pages[1].granule, 2880);
        let pieces = pages[1].pieces();
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0], (&long[..], true));
        assert_eq!(pieces[1], (&b""[..], true));
        assert_eq!(pieces[2], (&b"x"[..], true));
    }

    #[test]
    fn a_full_page_is_written_before_the_packet_that_overflows_it() {
        let mut w = PageWriter::new(Vec::new(), 1);
        for i in 0..MAX_SEGMENTS {
            w.add(&[1], i as i64).unwrap();
        }
        assert_eq!(w.pending_packets(), MAX_SEGMENTS);
        w.add(&[2], 999).unwrap();
        assert_eq!(w.pending_packets(), 1);
        w.write_page(0).unwrap();
        let pages = read_all(&w.into_inner());
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].granule, MAX_SEGMENTS as i64 - 1);
        assert_eq!(pages[1].granule, 999);
    }

    #[test]
    fn a_page_cut_short_reads_as_the_end() {
        let mut w = PageWriter::new(Vec::new(), 1);
        w.add(b"one", 1).unwrap();
        w.write_page(0).unwrap();
        w.add(b"two", 2).unwrap();
        w.write_page(0).unwrap();
        let bytes = w.into_inner();
        assert_eq!(read_all(&bytes[..bytes.len() - 1]).len(), 1);
    }

    #[test]
    fn corruption_fails_the_checksum() {
        let mut w = PageWriter::new(Vec::new(), 1);
        w.add(b"data", 1).unwrap();
        w.write_page(0).unwrap();
        let mut bytes = w.into_inner();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(read_page(&mut Cursor::new(&bytes)).is_err());
        assert!(read_page(&mut Cursor::new(b"RIFF....WAVEfmt nope nope n")).is_err());
    }
}
