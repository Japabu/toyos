use alloc::vec::Vec;

pub trait BlockDevice {
    fn sector_size(&self) -> u32;
    fn read_sector(&mut self, lba: u64, buf: &mut [u8]);
    fn write_sector(&mut self, lba: u64, buf: &[u8]);
}

pub struct Disk<T: BlockDevice> {
    dev: T,
    sector_size: u64,
    cache_lba: Option<u64>,
    cache_buf: Vec<u8>,
    cache_dirty: bool,
}

impl<T: BlockDevice> Disk<T> {
    pub fn new(dev: T) -> Self {
        let sector_size = dev.sector_size() as u64;
        Self {
            dev,
            sector_size,
            cache_lba: None,
            cache_buf: alloc::vec![0u8; sector_size as usize],
            cache_dirty: false,
        }
    }

    pub fn flush(&mut self) {
        if self.cache_dirty {
            if let Some(lba) = self.cache_lba {
                self.dev.write_sector(lba, &self.cache_buf);
                self.cache_dirty = false;
            }
        }
    }

    fn ensure_sector(&mut self, lba: u64) {
        if self.cache_lba == Some(lba) {
            return;
        }
        self.flush();
        self.dev.read_sector(lba, &mut self.cache_buf);
        self.cache_lba = Some(lba);
        self.cache_dirty = false;
    }

    pub fn read(&mut self, offset: u64, buf: &mut [u8]) {
        let mut remaining = buf.len() as u64;
        let mut pos = offset;
        let mut buf_off: usize = 0;

        while remaining > 0 {
            let lba = pos / self.sector_size;
            let sector_off = (pos % self.sector_size) as usize;
            let chunk = core::cmp::min(remaining, self.sector_size - sector_off as u64) as usize;

            self.ensure_sector(lba);
            buf[buf_off..buf_off + chunk]
                .copy_from_slice(&self.cache_buf[sector_off..sector_off + chunk]);

            pos += chunk as u64;
            buf_off += chunk;
            remaining -= chunk as u64;
        }
    }

    pub fn write(&mut self, offset: u64, buf: &[u8]) {
        let mut remaining = buf.len() as u64;
        let mut pos = offset;
        let mut buf_off: usize = 0;

        while remaining > 0 {
            let lba = pos / self.sector_size;
            let sector_off = (pos % self.sector_size) as usize;
            let chunk = core::cmp::min(remaining, self.sector_size - sector_off as u64) as usize;

            self.ensure_sector(lba);
            self.cache_buf[sector_off..sector_off + chunk]
                .copy_from_slice(&buf[buf_off..buf_off + chunk]);
            self.cache_dirty = true;

            pos += chunk as u64;
            buf_off += chunk;
            remaining -= chunk as u64;
        }
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

    #[test]
    fn basic_write_read() {
        let mem = MemDisk::new(16, 512);
        let mut disk = Disk::new(mem);
        disk.write(0, b"Hello Disk!!");
        disk.flush();
        let mut buf = [0u8; 12];
        disk.read(0, &mut buf);
        assert_eq!(&buf, b"Hello Disk!!");
    }

    #[test]
    fn cross_sector_write() {
        let mem = MemDisk::new(16, 512);
        let mut disk = Disk::new(mem);
        let ss = disk.sector_size;
        disk.write(ss - 4, b"CROSSSECTOR!");
        disk.flush();
        let mut buf = [0u8; 12];
        disk.read(ss - 4, &mut buf);
        assert_eq!(&buf, b"CROSSSECTOR!");
    }

    #[test]
    fn no_data_corruption() {
        let mem = MemDisk::new(16, 512);
        let mut disk = Disk::new(mem);
        disk.write(0, b"Hello Disk!!");
        disk.flush();

        let ss = disk.sector_size;
        disk.write(ss - 4, b"CROSSSECTOR!");
        disk.flush();

        let mut buf = [0u8; 12];
        disk.read(0, &mut buf);
        assert_eq!(&buf, b"Hello Disk!!");
    }

    #[test]
    fn multi_sector_write() {
        let mem = MemDisk::new(16, 512);
        let mut disk = Disk::new(mem);
        let ss = disk.sector_size;
        let big = alloc::vec![0xABu8; (ss * 3) as usize];
        disk.write(0, &big);
        disk.flush();
        let mut big_read = alloc::vec![0u8; (ss * 3) as usize];
        disk.read(0, &mut big_read);
        assert_eq!(big_read, big);
    }
}
