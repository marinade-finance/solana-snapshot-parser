pub trait SliceAt<T> {
    fn slice_at(&self, start: usize, length: usize) -> anyhow::Result<&[T]>;
}

impl<T> SliceAt<T> for [T] {
    fn slice_at(&self, start: usize, length: usize) -> anyhow::Result<&[T]> {
        self.get(start..start + length).ok_or_else(|| {
            anyhow::anyhow!(
                "SliceAt out of range: start={start}, length={length}, slice_length={}",
                self.len()
            )
        })
    }
}
