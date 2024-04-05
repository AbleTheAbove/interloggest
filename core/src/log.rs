//! User facing functions for readidng and writing to interlog replicas.
//! This can be considered the "top level" of the library.

use alloc::boxed::Box;
use alloc::string::String;

use crate::fixvec;
use crate::fixvec::FixVec;
use crate::log_id::LogID;
use crate::region;
use crate::{disk, event};
use event::Event;
use region::Region;

#[derive(Debug)]
pub struct EnqueueErr(fixvec::Overflow);

#[derive(Debug)]
pub enum CommitErr {
	Disk(disk::AppendErr),
	ReadCache(region::WriteErr),
	TxnWriteBufHasNoEvents,
	KeyIndex(fixvec::Overflow),
}

type WriteRes = Result<(), CommitErr>;

#[derive(Debug)]
pub struct Read<'a> {
	pub cache_hit: bool,
	pub event: Event<'a>,
}

/// A fixed sized structure that caches the latest entries in the log
/// (LIFO caching). The assumption is that things recently added are most
/// likely to be read out again.
///
/// To do this I'm using a single circular buffer, with two "write pointers".
/// At any given point in time the buffer will have two contiguous segments,
/// populated with events.
///
/// The reason to keep the two segments contiguous is so they can be easily
/// memcpy'd. So no event is split by the circular buffer.
///
/// There is a Top Segment, and a Bottom Segment.
///
/// We start with a top segment. The bottom segement gets created when we
/// reach the end of the circular buffer and need to wrap around, eating into
/// the former top segment.
///
/// Example:
/// ```text
/// ┌---┬---┬---┬---┬---┬---┬---┬---┬---┬---┬---┬---┬---┬---┬---┬---┐
/// | A | A | B | B | B |   | X | X | X | Y | Z | Z | Z | Z |   |   |
/// └---┴---┴---┴---┴---┴---┴---┴---┴---┴---┴---┴---┴---┴---┴---┴---┘
/// ```
///
/// The top segment contains events A and B, while the bottom segment contains
/// X, Y and Z
/// As more events are added, they will be appended after B, overwriting the
/// bottom segment, til it wraps round again.
struct ReadCache {
	mem: Box<[u8]>,
	/// Everything above this is in this cache
	pub logical_start: usize,
	a: Region,
	b: Region, // pos is always 0 but it's just easier
}

impl ReadCache {
	pub fn new(capacity: usize) -> Self {
		let mem = vec![0; capacity].into_boxed_slice();
		let logical_start = 0;
		let a = Region::ZERO;
		let b = Region::ZERO; // by definition B always starts at 0
		Self { mem, logical_start, a, b }
	}

	pub fn extend(
		region: &mut Region,
		dest: &mut [u8],
		src: &[u8],
	) -> Result<(), region::WriteErr> {
		let extension = Region::new(region.len, src.len());

		extension.write(dest, src)?;
		region.lengthen(src.len());
		Ok(())
	}

	fn overlapping_regions(&self) -> bool {
		self.b.end() > self.a.pos
	}

	pub fn update(&mut self, es: &[u8]) -> WriteRes {
		let result = match (self.a.empty(), self.b.empty()) {
			(true, true) => {
				self.set_logical_start(es);
				Self::extend(&mut self.a, &mut self.mem, es)
			}
			(false, true) => match Self::extend(&mut self.a, &mut self.mem, es)
			{
				Ok(()) => Ok(()),
				Err(region::WriteErr) => self.wrap_around(es),
			},
			(_, false) => self.wrap_around(es),
		};

		// Post conditions
		assert_eq!(self.b.pos, 0);
		assert!(!self.overlapping_regions());

		result.map_err(CommitErr::ReadCache)
	}

	pub fn read(&self, relative_byte_pos: usize) -> Option<Event<'_>> {
		let a_bytes = self.read_a();
		let e: Option<_> = event::read(a_bytes, relative_byte_pos);
		if let Some(_) = e {
			return e;
		}

		let relative_byte_pos = relative_byte_pos - a_bytes.len();
		return event::read(self.read_b(), relative_byte_pos);
	}

	fn set_logical_start(&mut self, es: &[u8]) {
		let first_event = event::read(es, 0).expect("no event found at 0");
		self.logical_start = first_event.id.logical_pos;
	}

	fn wrap_around(&mut self, es: &[u8]) -> Result<(), region::WriteErr> {
		match self.new_a_pos(es) {
			// Truncate A and write to B
			Some(new_a_pos) => {
				self.a.change_pos(new_a_pos);
				Self::extend(&mut self.b, self.mem.as_mut(), es)
			}
			// We've searched past the end of A and found nothing.
			// B is now A
			None => {
				self.a = Region::new(0, self.b.end());
				self.b = Region::ZERO;
				match Self::extend(&mut self.a, self.mem.as_mut(), es) {
					Ok(()) => Ok(()),
					// the new A cannot fit, erase it.
					// would not occur with buf size 2x or more max event size
					Err(region::WriteErr) => {
						self.a = Region::ZERO;
						self.set_logical_start(es);
						Self::extend(&mut self.a, self.mem.as_mut(), es)
					}
				}
			}
		}
	}

	fn new_a_pos(&self, es: &[u8]) -> Option<usize> {
		let new_b_end = self.b.end() + es.len();
		event::View::new(self.read_a())
			.scan(0, |offset, e| {
				*offset += e.on_disk_size();
				Some(*offset)
			})
			.find(|&offset| offset > new_b_end)
	}

	fn read_a(&self) -> &[u8] {
		self.a.read(&self.mem).expect("a range to be correct")
	}

	fn read_b(&self) -> &[u8] {
		self.b.read(&self.mem).expect("b range to be correct")
	}
}

pub struct Config {
	pub read_cache_capacity: usize,
	pub key_index_capacity: usize,
	pub txn_write_buf_capacity: usize,
	pub disk_read_buf_capacity: usize,
}

pub struct Log {
	id: LogID,
	/// Still counts as "static allocation" as only allocating in constructor
	path: String,
	disk: disk::Log,
	/// Keeps track of the disk
	byte_len: usize,
	read_cache: ReadCache,
	/// The entire index in memory, like bitcask's KeyDir
	/// Maps logical indices to disk offsets
	key_index: FixVec<usize>,
	/// This stores all the events w/headers, contiguously, which means only
	/// one syscall is required to write to disk.
	txn_write_buf: FixVec<u8>,
	/// Written to when a value is not in the read_cache
	disk_read_buf: FixVec<u8>,
}

impl Log {
	pub fn new(dir_path: &str, config: Config) -> rustix::io::Result<Self> {
		let id = LogID::new(&mut rand::thread_rng());
		let path = format!("{dir_path}/{id}");
		let disk = disk::Log::open(&path)?;

		Ok(Self {
			id,
			path,
			disk,
			byte_len: 0,
			read_cache: ReadCache::new(config.read_cache_capacity),
			key_index: FixVec::new(config.key_index_capacity),
			txn_write_buf: FixVec::new(config.txn_write_buf_capacity),
			disk_read_buf: FixVec::new(config.disk_read_buf_capacity),
		})
	}

	pub fn enqueue(&mut self, payload: &[u8]) -> Result<(), EnqueueErr> {
		let logical_pos = self.key_index.len();
		let e =
			Event { id: event::ID { origin: self.id, logical_pos }, payload };

		event::append(&mut self.txn_write_buf, &e).map_err(EnqueueErr)
	}

	pub fn commit(&mut self) -> Result<(), CommitErr> {
		assert_eq!(self.byte_len % 8, 0);

		if event::HEADER_SIZE > self.txn_write_buf.len() {
			return Err(CommitErr::TxnWriteBufHasNoEvents);
		}

		let bytes_flushed =
			self.disk.append(&self.txn_write_buf).map_err(CommitErr::Disk)?;

		self.read_cache.update(&self.txn_write_buf)?;

		// Disk offsets recorded in the Key Index always lag behind by one
		let mut disk_offset = self.byte_len;

		for e in event::View::new(&self.txn_write_buf) {
			self.key_index.push(disk_offset).map_err(CommitErr::KeyIndex)?;
			disk_offset += e.on_disk_size();
		}

		self.byte_len += bytes_flushed;
		assert!(self.byte_len % 8 == 0);
		Ok(self.txn_write_buf.clear())
	}

	pub fn read(&mut self, logical_pos: usize) -> Option<Read<'_>> {
		let byte_start = self.key_index[self.read_cache.logical_start];
		let byte_pos = self.key_index.get(logical_pos).cloned()?;

		match byte_pos.checked_sub(byte_start) {
			Some(relative_byte_pos) => {
				// If it's not in here, that means it doesn't exist at all
				self.read_cache
					.read(relative_byte_pos)
					.map(|event| Read { cache_hit: true, event })
			}
			None => {
				// read from disk
				let next_byte_pos =
					self.key_index.get(logical_pos + 1).cloned()?;
				let len = next_byte_pos
					.checked_sub(byte_pos)
					.expect("key index must always be in sorted order");
				self.disk_read_buf
					.resize(len)
					.expect("disk read buf should fit event");
				self.disk
					.read(&mut self.disk_read_buf, byte_pos)
					.expect("reading from disk failed");

				let event = event::read(&self.disk_read_buf, 0)
					.expect("Disk read buf did not contain a valid event");
				Some(Read { cache_hit: false, event })
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_utils::*;
	use pretty_assertions::assert_eq;
	use proptest::prelude::*;
	use tempfile::TempDir;

	#[test]
	fn log() {
		let tmp_dir = TempDir::with_prefix("interlog-").unwrap();
		let tmp_dir_path = tmp_dir.path().to_string_lossy().into_owned();

		let mut log = Log::new(
			&tmp_dir_path,
			Config {
				read_cache_capacity: 127,
				key_index_capacity: 0x10000,
				txn_write_buf_capacity: 512,
				disk_read_buf_capacity: 256,
			},
		)
		.unwrap();

		log.enqueue(b"I have known the arcane law").unwrap();
		log.commit().unwrap();

		assert_eq!(
			log.read(0).unwrap().event.payload,
			b"I have known the arcane law"
		);

		log.enqueue(b"On strange roads, such visions met").unwrap();
		log.commit().unwrap();

		assert_eq!(
			log.read(1).unwrap().event.payload,
			b"On strange roads, such visions met"
		);
	}

	/*
	proptest! {
		// TODO: change stream max to reveal bugs
		#[test]
		fn rw_log(ess in arb_local_events_stream(1, 16, 16)) {
			let tmp_dir = TempDir::with_prefix("interlog-").unwrap();

			let tmp_dir_path =
				tmp_dir
				.path()
				.to_string_lossy().into_owned();

			let mut rng = rand::thread_rng();
			let config = Config {
				txn_write_buf_capacity: 512,
				read_cache_capacity: 16 * 32,
				key_index_capacity: 0x10000,
			};

			let mut log = Log::new(&tmp_dir_path, &mut rng, &config)
				.expect("failed to open file");

			for es in ess {
				for e in es {
					log.enqueue(&e).expect("failed to write to replica");
				}

				log.commit().unwrap();

			}
		}
	}
	*/
}