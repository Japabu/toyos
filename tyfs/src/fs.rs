use alloc::string::String;
use alloc::vec::Vec;

use crate::disk::{BlockDevice, Disk};

const MAGIC: [u8; 4] = *b"TYFS";
const VERSION: u32 = 1;
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
//   [0]      flags (0=free, 1=in-use)
//   [1..32]  name (null-terminated)
//   [32..40] offset (byte offset of file data)
//   [40..48] size (file size in bytes)
//   [48..64] reserved

pub struct SimpleFs<T: BlockDevice> {
    disk: Disk<T>,
    disk_size: u64,
    data_end: u64,
    toc_start: u64,
}

impl<T: BlockDevice> SimpleFs<T> {
    pub fn format(mut disk: Disk<T>, disk_size: u64) -> Self {
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

    pub fn mount(mut disk: Disk<T>) -> Option<Self> {
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

    pub fn into_disk(self) -> Disk<T> {
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

    fn find_entry(&mut self, name: &str) -> Option<u64> {
        let name_bytes = name.as_bytes();
        let mut offset = self.toc_start;
        while offset + ENTRY_SIZE <= self.disk_size {
            let entry = self.read_entry(offset);
            if entry[0] == 1 {
                let end = entry[1..32].iter().position(|&b| b == 0).unwrap_or(31);
                if &entry[1..1 + end] == name_bytes {
                    return Some(offset);
                }
            }
            offset += ENTRY_SIZE;
        }
        None
    }

    pub fn create(&mut self, name: &str, data: &[u8]) -> bool {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > 30 {
            return false;
        }

        let needed = data.len() as u64 + ENTRY_SIZE;
        if self.data_end + needed > self.toc_start {
            return false;
        }

        // Write file data at data_end
        let file_offset = self.data_end;
        self.disk.write(file_offset, data);

        // Build ToC entry
        let new_toc = self.toc_start - ENTRY_SIZE;
        let mut entry = [0u8; ENTRY_SIZE as usize];
        entry[0] = 1; // in-use
        entry[1..1 + name_bytes.len()].copy_from_slice(name_bytes);
        entry[32..40].copy_from_slice(&file_offset.to_le_bytes());
        entry[40..48].copy_from_slice(&(data.len() as u64).to_le_bytes());

        self.disk.write(new_toc, &entry);

        // Update pointers
        self.data_end += data.len() as u64;
        self.toc_start = new_toc;
        self.write_header();
        true
    }

    pub fn read_file(&mut self, name: &str) -> Option<Vec<u8>> {
        let entry_offset = self.find_entry(name)?;
        let entry = self.read_entry(entry_offset);
        let offset = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let size = u64::from_le_bytes(entry[40..48].try_into().unwrap());

        let mut buf = alloc::vec![0u8; size as usize];
        self.disk.read(offset, &mut buf);
        Some(buf)
    }

    pub fn delete(&mut self, name: &str) -> bool {
        if let Some(entry_offset) = self.find_entry(name) {
            self.disk.write(entry_offset, &[0u8; 1]); // clear flags byte
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
            if entry[0] == 1 {
                let end = entry[1..32].iter().position(|&b| b == 0).unwrap_or(31);
                let name = core::str::from_utf8(&entry[1..1 + end])
                    .unwrap_or("")
                    .into();
                let size = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                result.push((name, size));
            }
            offset += ENTRY_SIZE;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemDisk {
        data: Vec<u8>,
        sector_size: u32,
    }

    impl MemDisk {
        fn new(num_sectors: usize, sector_size: u32) -> Self {
            Self {
                data: alloc::vec![0u8; num_sectors * sector_size as usize],
                sector_size,
            }
        }

        fn size(&self) -> u64 {
            self.data.len() as u64
        }
    }

    impl BlockDevice for MemDisk {
        fn sector_size(&self) -> u32 {
            self.sector_size
        }

        fn read_sector(&mut self, lba: u64, buf: &mut [u8]) {
            let offset = lba as usize * self.sector_size as usize;
            let len = buf.len().min(self.sector_size as usize);
            buf[..len].copy_from_slice(&self.data[offset..offset + len]);
        }

        fn write_sector(&mut self, lba: u64, buf: &[u8]) {
            let offset = lba as usize * self.sector_size as usize;
            let len = buf.len().min(self.sector_size as usize);
            self.data[offset..offset + len].copy_from_slice(&buf[..len]);
        }
    }

    fn make_fs() -> SimpleFs<MemDisk> {
        let mem = MemDisk::new(64, 512);
        let size = mem.size();
        let disk = Disk::new(mem);
        SimpleFs::format(disk, size)
    }

    #[test]
    fn format_and_mount() {
        let mem = MemDisk::new(64, 512);
        let size = mem.size();
        let disk = Disk::new(mem);
        let fs = SimpleFs::format(disk, size);

        // Re-mount from the same disk
        let fs2 = SimpleFs::mount(fs.into_disk());
        assert!(fs2.is_some());
        let fs2 = fs2.unwrap();
        assert_eq!(fs2.disk_size, size);
        assert_eq!(fs2.data_end, HEADER_SIZE);
        assert_eq!(fs2.toc_start, size);
    }

    #[test]
    fn create_and_read() {
        let mut fs = make_fs();
        assert!(fs.create("hello.txt", b"Hello, world!"));
        let data = fs.read_file("hello.txt");
        assert_eq!(data.as_deref(), Some(b"Hello, world!".as_slice()));
    }

    #[test]
    fn multiple_files_and_list() {
        let mut fs = make_fs();
        assert!(fs.create("a.txt", b"aaa"));
        assert!(fs.create("b.txt", b"bbbbb"));
        assert!(fs.create("c.txt", b"c"));

        let files = fs.list();
        assert_eq!(files.len(), 3);

        // ToC grows downward, so first entry (c.txt) is at lowest offset
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
        let mem = MemDisk::new(1, 512); // tiny disk: 512 bytes
        let size = mem.size();
        let disk = Disk::new(mem);
        let mut fs = SimpleFs::format(disk, size);

        // 512 - 64 (header) = 448 bytes free
        // Creating a file needs data.len() + 64 (entry) bytes
        // So max data = 448 - 64 = 384
        assert!(fs.create("big.txt", &[0xAA; 384]));
        // No room left
        assert!(!fs.create("nope.txt", b"x"));
    }

    #[test]
    fn mount_reads_existing_files() {
        let mut fs = make_fs();
        fs.create("persist.txt", b"saved data");

        // Re-mount
        let mut fs2 = SimpleFs::mount(fs.into_disk()).unwrap();
        let data = fs2.read_file("persist.txt");
        assert_eq!(data.as_deref(), Some(b"saved data".as_slice()));
    }
}
