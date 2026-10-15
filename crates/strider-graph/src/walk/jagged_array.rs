/// Lists of lists stored flat: row `i` is `items[start[i]..start[i + 1]]`.
pub(super) struct JaggedArray {
    pub(super) start: Vec<u32>,
    pub(super) items: Vec<u32>,
}

impl JaggedArray {
    /// Creates an empty list, with one open row to push to.
    pub(super) fn new() -> Self {
        JaggedArray { start: vec![0], items: Vec::new() }
    }

    /// Closes the open row and opens the next.
    pub(super) fn end_row(&mut self) {
        self.start.push(self.items.len() as u32);
    }

    /// Returns the number of closed rows.
    pub(super) fn len(&self) -> usize {
        self.start.len() - 1
    }

    /// Returns row `i`.
    pub(super) fn row(&self, i: usize) -> &[u32] {
        &self.items[self.start[i] as usize..self.start[i + 1] as usize]
    }

    /// Makes a list of `rows` rows from `(row, item)` pairs, keeping each row's items in order.
    pub(super) fn grouped(rows: usize, pairs: impl Iterator<Item = (u32, u32)> + Clone) -> Self {
        let mut start = vec![0; rows + 1];
        for (row, _) in pairs.clone() {
            start[row as usize + 1] += 1;
        }
        for i in 1..=rows {
            start[i] += start[i - 1];
        }
        let mut items = vec![0; start[rows] as usize];
        for (row, item) in pairs {
            items[start[row as usize] as usize] = item;
            start[row as usize] += 1;
        }
        // Each start has moved to its row's end, which is where the next row starts.
        start.rotate_right(1);
        start[0] = 0;
        JaggedArray { start, items }
    }
}
