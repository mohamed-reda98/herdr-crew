use std::{
    fs::File,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
};

/// Follows a file that another process appends to, yielding only complete lines.
pub struct Tail {
    path: PathBuf,
    reader: Option<BufReader<File>>,
    pending: Vec<u8>,
    position: u64,
}

impl Tail {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            reader: None,
            pending: Vec::new(),
            position: 0,
        }
    }

    /// Follows another file instead, as long as the first was never found; says whether it did.
    pub fn redirect(&mut self, path: &Path) -> bool {
        let applies = self.reader.is_none() && path != self.path;
        if applies {
            self.path = path.to_owned();
        }
        applies
    }

    fn open(&mut self) -> io::Result<()> {
        if self.reader.is_none() {
            match File::open(&self.path) {
                Ok(file) => self.reader = Some(BufReader::new(file)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn lines(&mut self) -> io::Result<Vec<Vec<u8>>> {
        self.open()?;
        // A file rewritten from the start would otherwise be read from the middle.
        if let Some(reader) = &self.reader {
            if reader.get_ref().metadata()?.len() < self.position {
                self.reader = None;
                self.pending.clear();
                self.position = 0;
                self.open()?;
            }
        }
        let mut lines = Vec::new();
        while let Some(reader) = self.reader.as_mut() {
            let read = reader.read_until(b'\n', &mut self.pending)?;
            if read == 0 {
                break;
            }
            self.position += read as u64;
            // EOF is temporary: a later append may complete this JSON or UTF-8 code point.
            if self.pending.last() != Some(&b'\n') {
                break;
            }
            lines.push(std::mem::take(&mut self.pending));
        }
        Ok(lines)
    }
}
