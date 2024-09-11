pub(crate) mod duration;
pub(crate) mod frame_count;

pub(crate) trait SwapStrategy: Sync + Send + 'static {
    fn should_swap(&self, frames_in_wal: usize) -> bool;
    fn swapped(&self);

    fn and<O: SwapStrategy>(self, other: O) -> And<Self, O>
    where
        Self: Sized,
    {
        And(self, other)
    }

    fn or<O: SwapStrategy>(self, other: O) -> Or<Self, O>
    where
        Self: Sized,
    {
        Or(self, other)
    }
}

pub struct And<A, B>(A, B);

impl<A, B> SwapStrategy for And<A, B>
where
    A: SwapStrategy,
    B: SwapStrategy,
{
    #[inline(always)]
    fn should_swap(&self, frames_in_wal: usize) -> bool {
        self.0.should_swap(frames_in_wal) && self.1.should_swap(frames_in_wal)
    }

    #[inline(always)]
    fn swapped(&self) {
        self.0.swapped();
        self.1.swapped();
    }
}

pub struct Or<A, B>(A, B);

impl<A, B> SwapStrategy for Or<A, B>
where
    A: SwapStrategy,
    B: SwapStrategy,
{
    #[inline(always)]
    fn should_swap(&self, frames_in_wal: usize) -> bool {
        self.0.should_swap(frames_in_wal) || self.1.should_swap(frames_in_wal)
    }

    #[inline(always)]
    fn swapped(&self) {
        self.0.swapped();
        self.1.swapped();
    }
}
