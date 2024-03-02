//! Fixed capacity data structures, that do not allocate when modified.
use std::slice::SliceIndex;

fn uninit_boxed_slice<T>(size: usize) -> Box<[T]> {
	let mut result = Vec::with_capacity(size);
	#[allow(clippy::uninit_vec)]
	unsafe {
		result.set_len(size)
	};
	result.into_boxed_slice()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
	pub pos: usize,
	pub len: usize,
	pub end: usize
}

impl Segment {
	pub const fn new(pos: usize, len: usize) -> Self {
		Self { pos, len, end: pos + len }
	}

	pub const ZERO: Self = Self::new(0, 0);

	pub fn lengthen(&mut self, n: usize) {
		self.len += n;
		self.end = self.pos + self.len;
	}

	/// Set pos to new_pos, while leaving the end the same
	pub fn change_pos(&mut self, new_pos: usize) {
		self.pos = new_pos;
		self.len = self.end - new_pos;
	}

	pub fn next(&self, len: usize) -> Self {
		Self::new(self.pos + self.len, len)
	}

	pub fn range(&self) -> core::ops::Range<usize> {
		self.pos..self.end
	}
}

pub trait Segmentable<T> {
	fn segment(&self, index: &Segment) -> Option<&[T]>;
}

/// Fixed Capacity Vector
/// Tigerstyle: There IS a limit
pub struct FixVec<T> {
	elems: alloc::boxed::Box<[T]>,
	len: usize
}

#[derive(Debug)]
pub struct FixVecOverflow;
pub type FixVecRes = Result<(), FixVecOverflow>;

impl<T> FixVec<T> {
	#[allow(clippy::uninit_vec)]
	pub fn new(capacity: usize) -> FixVec<T> {
		let elems = uninit_boxed_slice(capacity);
		assert_eq!(std::mem::size_of_val(&elems), 16);
		Self { elems, len: 0 }
	}

	#[inline]
	pub fn capacity(&self) -> usize {
		self.elems.len()
	}

	#[inline]
	pub fn clear(&mut self) {
		self.len = 0;
	}

	#[inline]
	fn len(&self) -> usize {
		self.len
	}

	fn check_capacity(&self, new_len: usize) -> FixVecRes {
		(self.capacity() >= new_len).then_some(()).ok_or(FixVecOverflow)
	}

	pub fn push(&mut self, value: T) -> FixVecRes {
		let new_len = self.len + 1;
		self.check_capacity(new_len)?;
		self.elems[self.len] = value;
		self.len = new_len;
		Ok(())
	}

	pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) -> FixVecRes {
		for elem in iter {
			self.push(elem)?;
		}

		Ok(())
	}

	fn insert(&mut self, index: usize, element: T) -> FixVecRes {
		self.check_capacity(index + 1)?;
		self.elems[index] = element;
		Ok(())
	}

	fn get<I>(&self, index: I) -> Option<&<I as SliceIndex<[T]>>::Output>
	where
		I: SliceIndex<[T]>
	{
		self.elems[..self.len].get(index)
	}
}

impl<T: Clone + core::fmt::Debug> FixVec<T> {
	pub fn resize(&mut self, new_len: usize, value: T) -> FixVecRes {
		self.check_capacity(new_len)?;

		if new_len > self.len {
			self.elems[self.len..new_len].fill(value);
		}

		self.len = new_len;

		Ok(())
	}
}

impl<T: Copy> FixVec<T> {
	pub fn extend_from_slice(&mut self, other: &[T]) -> FixVecRes {
		let new_len = self.len + other.len();
		self.check_capacity(new_len)?;
		self.elems[self.len..new_len].copy_from_slice(other);
		self.len = new_len;
		Ok(())
	}
}

impl<T> std::ops::Deref for FixVec<T> {
	type Target = [T];

	fn deref(&self) -> &Self::Target {
		&self.elems[..self.len]
	}
}

impl<T> std::ops::DerefMut for FixVec<T> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.elems[..self.len]
	}
}

impl<T> Segmentable<T> for FixVec<T> {
	fn segment(&self, index: &Segment) -> Option<&[T]> {
		self.elems[..self.len].get(index.range())
	}
}

#[derive(Debug)]
pub struct CircBufWrapAround;

pub struct CircBuf<T> {
	buf: Box<[T]>,
	len: usize,
	write_idx: usize
}

impl<T> CircBuf<T> {
	pub fn new(capacity: usize) -> Self {
		let buffer = uninit_boxed_slice(capacity);
		let len = 0;
		let write_idx = 0;
		Self { buf: buffer, len, write_idx }
	}

	#[inline]
	pub fn capacity(&self) -> usize {
		self.buf.len()
	}

	#[inline]
	pub fn write_idx(&self) -> usize {
		self.write_idx
	}

	pub fn push(&mut self, item: T) {
		self.buf[self.write_idx] = item;
		self.write_idx = (self.write_idx + 1) % self.capacity();
		self.len = core::cmp::min(self.len + 1, self.capacity());
	}

	pub fn get(&self, index: usize) -> Option<&T> {
		if self.len == 0 {
			return None;
		}
		let index = (index + self.write_idx).wrapping_rem_euclid(self.len);
		(self.len > index).then(|| &self.buf[index])
	}
}

impl<T: Copy> CircBuf<T> {
	// Will fail if it causes a wrap around
	// slice should remain contiguous in memory
	// TODO: this should just start writing from the beginning if it wraps around?
	pub fn extend_from_slice(
		&mut self,
		slice: &[T]
	) -> Result<(), CircBufWrapAround> {
		let contiguous_space_left = self.capacity() - self.write_idx;
		if contiguous_space_left > slice.len() {
			self.buf[..self.len].copy_from_slice(slice);
		}

		Err(CircBufWrapAround)
	}
}

impl<T> CircBuf<T> {
	fn iter(&self) -> CircBufIterator<'_, T> {
		CircBufIterator { circ_buf: self, index: 0 }
	}
}

struct CircBufIterator<'a, T> {
	circ_buf: &'a CircBuf<T>,
	index: usize
}

impl<'a, T> Iterator for CircBufIterator<'a, T> {
	type Item = &'a T;

	fn next(&mut self) -> Option<Self::Item> {
		// This check prevents going around the circle infinitely
		if self.index < self.circ_buf.len {
			let item = self.circ_buf.get(self.index)?;
			self.index += 1;
			Some(item)
		} else {
			None
		}
	}
}

/// Implementation of Simon Cookess bi-partite circular buffer
pub struct BipBuf<T> {
	buf: Box<[T]>,
	a_start: usize,
	a_end: usize,
	b_start: usize,
	b_end: usize,
	reserve_start: usize,
	reserve_end: usize
}

impl<T> BipBuf<T> {
	pub fn new(capacity: usize) -> Self {
		Self {
			buf: uninit_boxed_slice(capacity),
			a_start: 0,
			a_end: 0,
			b_start: 0,
			b_end: 0,
			reserve_start: 0,
			reserve_end: 0
		}
	}

	pub fn clear(&mut self) {
		self.a_start = 0;
		self.a_end = 0;
		self.b_start = 0;
		self.b_end = 0;
		self.reserve_start = 0;
		self.reserve_end = 0;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pretty_assertions::assert_eq;

	#[test]
	fn circ_buf() {
		let mut cb = CircBuf::new(4);

		// Preconditions
		assert_eq!(cb.iter().collect::<String>(), "");
		assert_eq!(cb.write_idx, 0);
		assert_eq!(cb.len, 0);

		cb.push('s');
		assert_eq!(cb.iter().copied().collect::<String>(), "s");
		assert_eq!(cb.write_idx, 1);
		assert_eq!(cb.len, 1);

		cb.push('i');
		assert_eq!(cb.iter().copied().collect::<String>(), "si");
		assert_eq!(cb.write_idx, 2);
		assert_eq!(cb.len, 2);

		cb.push('l');
		assert_eq!(cb.iter().collect::<String>(), "sil");
		assert_eq!(cb.write_idx, 3);
		assert_eq!(cb.len, 3);

		cb.push('m');
		assert_eq!(cb.iter().collect::<String>(), "silm");
		assert_eq!(cb.write_idx, 0);
		assert_eq!(cb.len, 4);

		cb.push('a');
		assert_eq!(cb.iter().collect::<String>(), "ilma");
		assert_eq!(cb.write_idx, 1);
		assert_eq!(cb.len, 4);

		cb.push('r');
		assert_eq!(cb.iter().collect::<String>(), "lmar");
		assert_eq!(cb.write_idx, 2);
		assert_eq!(cb.len, 4);
	}
}