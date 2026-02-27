use alloc::string::String;
use alloc::vec::Vec;

use crate::disk::Disk;

const MAGIC: [u8; 4] = *b"TYFS";
const VERSION: u32 = 2;
const HEADER_SIZE: u64 = 64;
const ENTRY_SIZE: u64 = 64;

// Header layout (64 bytes at offset 0):
//   [0..4]   magic
//   [4..8]   version
//   [8..16]  disk_size
//   [16..24] data_end
//   [24..32] toc_start
//   [32..64] reserved

// ToC entry layout (64 bytes each, growing downward from end):
//   [0]       type (0=free, 1=file, 2=symlink)
//   [8..16]   name_offset (byte offset of name in data section)
//   [16..24]  name_len
//   [24..32]  data_offset (byte offset of file/symlink data)
//   [32..40]  data_len
//   [40..64]  reserved

pub struct SimpleFs<D: Disk> {
    disk: D,
    disk_size: u64,
    data_end: u64,
    toc_start: u64,
}

impl<D: Disk> SimpleFs<D> {
    pub fn format(mut disk: D, disk_size: u64) -> Self {
        let mut header = [0u8; HEADER_SIZE as usize];
        header[0..4].copy_from_slice(&MAGIC);
        header[4..8].copy_from_slice(&VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&disk_size.to_le_bytes());
        header[16..24].copy_from_slice(&HEADER_SIZE.to_le_bytes());
        header[24..32].copy_from_slice(&disk_size.to_le_bytes());
        disk.write(0, &header);
        disk.flush();

        Self {
            disk,
            disk_size,
            data_end: HEADER_SIZE,
            toc_start: disk_size,
        }
    }

    pub fn mount(mut disk: D) -> Option<Self> {
        let mut header = [0u8; HEADER_SIZE as usize];
        disk.read(0, &mut header);

        if header[0..4] != MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
        if version != VERSION {
            return None;
        }

        let disk_size = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let data_end = u64::from_le_bytes(header[16..24].try_into().unwrap());
        let toc_start = u64::from_le_bytes(header[24..32].try_into().unwrap());

        Some(Self {
            disk,
            disk_size,
            data_end,
            toc_start,
        })
    }

    pub fn into_disk(self) -> D {
        self.disk
    }

    fn write_header(&mut self) {
        let mut header = [0u8; HEADER_SIZE as usize];
        header[0..4].copy_from_slice(&MAGIC);
        header[4..8].copy_from_slice(&VERSION.to_le_bytes());
        header[8..16].copy_from_slice(&self.disk_size.to_le_bytes());
        header[16..24].copy_from_slice(&self.data_end.to_le_bytes());
        header[24..32].copy_from_slice(&self.toc_start.to_le_bytes());
        self.disk.write(0, &header);
        self.disk.flush();
    }

    fn read_entry(&mut self, entry_offset: u64) -> [u8; ENTRY_SIZE as usize] {
        let mut buf = [0u8; ENTRY_SIZE as usize];
        self.disk.read(entry_offset, &mut buf);
        buf
    }

    fn entry_name(&mut self, entry: &[u8; ENTRY_SIZE as usize]) -> String {
        let name_offset = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        let name_len = u64::from_le_bytes(entry[16..24].try_into().unwrap());
        let mut buf = alloc::vec![0u8; name_len as usize];
        self.disk.read(name_offset, &mut buf);
        String::from_utf8(buf).unwrap_or_default()
    }

    fn find_entry(&mut self, name: &str) -> Option<u64> {
        let mut offset = self.toc_start;
        while offset + ENTRY_SIZE <= self.disk_size {
            let entry = self.read_entry(offset);
            if entry[0] != 0 && self.entry_name(&entry) == name {
                return Some(offset);
            }
            offset += ENTRY_SIZE;
        }
        None
    }

    pub fn create(&mut self, name: &str, data: &[u8]) -> bool {
        self.create_entry(name, 1, data)
    }

    pub fn create_symlink(&mut self, name: &str, target: &str) -> bool {
        self.create_entry(name, 2, target.as_bytes())
    }

    pub fn read_link(&mut self, name: &str) -> Option<String> {
        let entry_offset = self.find_entry(name)?;
        let entry = self.read_entry(entry_offset);
        if entry[0] != 2 {
            return None;
        }
        let data_offset = u64::from_le_bytes(entry[24..32].try_into().unwrap());
        let data_len = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let mut buf = alloc::vec![0u8; data_len as usize];
        self.disk.read(data_offset, &mut buf);
        Some(String::from(core::str::from_utf8(&buf).ok()?))
    }

    fn create_entry(&mut self, name: &str, entry_type: u8, data: &[u8]) -> bool {
        let name_bytes = name.as_bytes();
        let total_data = name_bytes.len() as u64 + data.len() as u64;
        let needed = total_data + ENTRY_SIZE;
        if self.data_end + needed > self.toc_start {
            return false;
        }

        // Write name then data into the data section
        let name_offset = self.data_end;
        self.disk.write(name_offset, name_bytes);
        let data_offset = name_offset + name_bytes.len() as u64;
        self.disk.write(data_offset, data);

        // Write TOC entry
        let new_toc = self.toc_start - ENTRY_SIZE;
        let mut entry = [0u8; ENTRY_SIZE as usize];
        entry[0] = entry_type;
        entry[8..16].copy_from_slice(&name_offset.to_le_bytes());
        entry[16..24].copy_from_slice(&(name_bytes.len() as u64).to_le_bytes());
        entry[24..32].copy_from_slice(&data_offset.to_le_bytes());
        entry[32..40].copy_from_slice(&(data.len() as u64).to_le_bytes());
        self.disk.write(new_toc, &entry);

        self.data_end += total_data;
        self.toc_start = new_toc;
        self.write_header();
        true
    }

    pub fn read_file(&mut self, name: &str) -> Option<Vec<u8>> {
        let entry_offset = self.find_entry(name)?;
        let entry = self.read_entry(entry_offset);
        let data_offset = u64::from_le_bytes(entry[24..32].try_into().unwrap());
        let data_len = u64::from_le_bytes(entry[32..40].try_into().unwrap());

        let mut buf = Vec::new();
        if buf.try_reserve_exact(data_len as usize).is_err() {
            return None;
        }
        buf.resize(data_len as usize, 0u8);
        self.disk.read(data_offset, &mut buf);
        Some(buf)
    }

    pub fn delete(&mut self, name: &str) -> bool {
        if let Some(entry_offset) = self.find_entry(name) {
            self.disk.write(entry_offset, &[0u8; 1]);
            self.disk.flush();
            true
        } else {
            false
        }
    }

    pub fn list(&mut self) -> Vec<(String, u64)> {
        let mut result = Vec::new();
        let mut offset = self.toc_start;
        while offset + ENTRY_SIZE <= self.disk_size {
            let entry = self.read_entry(offset);
            if entry[0] != 0 {
                let name = self.entry_name(&entry);
                let data_len = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                result.push((name, data_len));
            }
            offset += ENTRY_SIZE;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::disk::Disk;

    struct MemDisk(std::vec::Vec<u8>);

    impl MemDisk {
        fn new(size: usize) -> Self {
            Self(std::vec![0u8; size])
        }
    }

    impl Disk for MemDisk {
        fn read(&mut self, offset: u64, buf: &mut [u8]) {
            let off = offset as usize;
            buf.copy_from_slice(&self.0[off..off + buf.len()]);
        }
        fn write(&mut self, offset: u64, buf: &[u8]) {
            let off = offset as usize;
            self.0[off..off + buf.len()].copy_from_slice(buf);
        }
        fn flush(&mut self) {}
    }

    fn make_fs() -> SimpleFs<MemDisk> {
        let size = 64 * 512;
        SimpleFs::format(MemDisk::new(size), size as u64)
    }

    #[test]
    fn format_and_mount() {
        let size = 64 * 512;
        let fs = SimpleFs::format(MemDisk::new(size), size as u64);

        let fs2 = SimpleFs::mount(fs.into_disk());
        assert!(fs2.is_some());
        let fs2 = fs2.unwrap();
        assert_eq!(fs2.disk_size, size as u64);
        assert_eq!(fs2.data_end, HEADER_SIZE);
        assert_eq!(fs2.toc_start, size as u64);
    }

    #[test]
    fn create_and_read() {
        let mut fs = make_fs();
        assert!(fs.create("hello.txt", b"Hello, world!"));
        let data = fs.read_file("hello.txt");
        assert_eq!(data.as_deref(), Some(b"Hello, world!".as_slice()));
    }

    #[test]
    fn long_filename() {
        let mut fs = make_fs();
        let long_name = "librustc_codegen_cranelift-1.95.0-dev.so";
        assert!(fs.create(long_name, b"elf data here"));
        let data = fs.read_file(long_name);
        assert_eq!(data.as_deref(), Some(b"elf data here".as_slice()));

        let files = fs.list();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, long_name);
    }

    #[test]
    fn multiple_files_and_list() {
        let mut fs = make_fs();
        assert!(fs.create("a.txt", b"aaa"));
        assert!(fs.create("b.txt", b"bbbbb"));
        assert!(fs.create("c.txt", b"c"));

        let files = fs.list();
        assert_eq!(files.len(), 3);

        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        assert!(names.contains(&"c.txt"));

        assert_eq!(fs.read_file("a.txt").as_deref(), Some(b"aaa".as_slice()));
        assert_eq!(fs.read_file("b.txt").as_deref(), Some(b"bbbbb".as_slice()));
        assert_eq!(fs.read_file("c.txt").as_deref(), Some(b"c".as_slice()));
    }

    #[test]
    fn delete_file() {
        let mut fs = make_fs();
        assert!(fs.create("rm_me.txt", b"gone"));
        assert!(fs.delete("rm_me.txt"));

        assert!(fs.read_file("rm_me.txt").is_none());
        assert_eq!(fs.list().len(), 0);
    }

    #[test]
    fn disk_full() {
        let size = 512;
        let mut fs = SimpleFs::format(MemDisk::new(size), size as u64);

        assert!(fs.create("big.txt", &[0xAA; 320]));
        assert!(!fs.create("nope.txt", b"x"));
    }

    #[test]
    fn symlinks() {
        let mut fs = make_fs();
        assert!(fs.create("target.txt", b"real data"));
        assert!(fs.create_symlink("link.txt", "target.txt"));

        // read_link returns target for symlinks, None for regular files
        assert_eq!(fs.read_link("link.txt").as_deref(), Some("target.txt"));
        assert_eq!(fs.read_link("target.txt"), None);

        // read_file on a symlink returns the raw target bytes (VFS resolves)
        assert_eq!(fs.read_file("link.txt").as_deref(), Some(b"target.txt".as_slice()));

        // Both show up in list
        assert_eq!(fs.list().len(), 2);
    }

    #[test]
    fn mount_reads_existing_files() {
        let mut fs = make_fs();
        fs.create("persist.txt", b"saved data");

        let mut fs2 = SimpleFs::mount(fs.into_disk()).unwrap();
        let data = fs2.read_file("persist.txt");
        assert_eq!(data.as_deref(), Some(b"saved data".as_slice()));
    }
}
