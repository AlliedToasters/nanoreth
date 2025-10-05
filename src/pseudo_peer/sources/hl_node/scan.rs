use crate::node::types::{BlockAndReceipts, EvmBlock};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    ops::RangeInclusive,
    path::{Path, PathBuf},
};
use tracing::warn;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LocalBlockAndReceipts(pub String, pub BlockAndReceipts);

pub struct ScanResult {
    pub path: PathBuf,
    pub next_expected_height: u64,
    pub new_blocks: Vec<BlockAndReceipts>,
    pub new_block_ranges: Vec<RangeInclusive<u64>>,
}

pub struct ScanOptions {
    pub start_height: u64,
    pub only_load_ranges: bool,
}

pub struct Scanner;

/// Stream for sequentially reading lines from a file.
///
/// This struct allows sequential iteration over lines over [Self::next] method.
/// It is resilient to cases where the line producer process is interrupted while writing:
/// - If a line is incomplete but still ends with a line ending, it is skipped: later, the fallback
///   block source will be used to retrieve the missing block.
/// - If a line does not end with a newline (i.e., the write was incomplete), the method returns
///   `None` to break out of the loop and avoid reading partial data.
/// - If a temporary I/O error occurs, the stream exits the loop without rewinding the cursor, which
///   will result in skipping ahead to the next unread bytes.
pub struct LineStream {
    path: PathBuf,
    reader: BufReader<File>,
}

impl LineStream {
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let reader = BufReader::with_capacity(1024 * 1024, File::open(path)?);
        Ok(Self { path: path.to_path_buf(), reader })
    }

    pub fn next(&mut self) -> Option<String> {
        let mut line_buffer = vec![];
        let Ok(size) = self.reader.read_until(b'\n', &mut line_buffer) else {
            // Temporary I/O error; restart the loop
            return None;
        };

        // Now cursor is right after the end of the line
        // On UTF-8 error, skip the line
        let Ok(mut line) = String::from_utf8(line_buffer) else {
            return Some(String::new());
        };

        // If line is not completed yet, return None so that we can break the loop
        if line.ends_with('\n') {
            if line.ends_with('\r') {
                line.pop();
            }
            line.pop();
            return Some(line);
        }

        // info!("Line is not completed yet: {}", line);
        if size != 0 {
            self.reader.seek(SeekFrom::Current(-(size as i64))).unwrap();
        }
        None
    }
}

impl Scanner {
    pub fn line_to_evm_block(line: &str) -> serde_json::Result<(BlockAndReceipts, u64)> {
        let LocalBlockAndReceipts(_, parsed_block): LocalBlockAndReceipts =
            serde_json::from_str(line)?;
        let height = match &parsed_block.block {
            EvmBlock::Reth115(b) => b.header.header.number,
        };
        Ok((parsed_block, height))
    }

    pub fn scan_hour_file(line_stream: &mut LineStream, options: ScanOptions) -> ScanResult {
        let mut new_blocks = Vec::new();
        let mut last_height = options.start_height;
        let mut block_ranges = Vec::new();
        let mut current_range: Option<(u64, u64)> = None;

        while let Some(line) = line_stream.next() {
            match Self::line_to_evm_block(&line) {
                Ok((parsed_block, height)) => {
                    if height >= options.start_height {
                        last_height = last_height.max(height);
                        if !options.only_load_ranges {
                            new_blocks.push(parsed_block);
                        }
                    }

                    match current_range {
                        Some((start, end)) if end + 1 == height => {
                            current_range = Some((start, height))
                        }
                        _ => {
                            if let Some((start, end)) = current_range.take() {
                                block_ranges.push(start..=end);
                            }
                            current_range = Some((height, height));
                        }
                    }
                }
                Err(_) => warn!("Failed to parse line: {}...", line.get(0..50).unwrap_or(&line)),
            }
        }

        if let Some((start, end)) = current_range {
            block_ranges.push(start..=end);
        }

        ScanResult {
            path: line_stream.path.clone(),
            next_expected_height: last_height + current_range.is_some() as u64,
            new_blocks,
            new_block_ranges: block_ranges,
        }
    }
}
