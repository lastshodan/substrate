// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! This module is composed of two structs: [`TimerApi`] and [`TimerWorker`]. Calling the [`timer`]
//! function returns a pair of [`TimerApi`] and [`TimerWorker`] that share some state.
//!
//! For more technical details and reasoning behind this design, see the analogous [`http`] module.

use sp_core::offchain;
use sp_core::offchain::Timestamp;
use sp_core::offchain::PollableId;
use sp_utils::mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender};

use core::cmp::{Ordering, Reverse};
use core::future::Future;
use core::pin::Pin;
use core::task::{self, Poll};
use core::time;
use std::collections::BinaryHeap;

use futures::Stream;
use futures_timer::Delay;

use super::timestamp;

pub use sp_core::offchain::TimerId;

pub fn timer(sink: TracingUnboundedSender<PollableId>) -> (TimerApi, TimerWorker) {
	let (to_worker, from_api) = tracing_unbounded("mpsc_ocw_timer_from");

	let worker = TimerWorker {
		ready_ids: sink,
		from_api,
		delay: None,
		ids: Default::default(),
	};

	let api = TimerApi {
		to_worker,
		next_id: TimerId(0),
	};

	(api, worker)
}

pub struct TimerApi {
	/// Used to enqueue new timer in the `TimerWorker`.
	to_worker: TracingUnboundedSender<(TimerId, Timestamp)>,
	/// Counter used to generate new timer IDs.
	next_id: TimerId,
}

impl TimerApi {
	/// Starts a new timer that resolves a `duration` from the current epoch.
	pub fn start_timer(&mut self, duration: offchain::Duration) -> TimerId {
		let id = self.next_id;
		self.next_id = TimerId(self.next_id.0 + 1);

		let timestamp = timestamp::now().add(duration);

		self.to_worker.unbounded_send((id, timestamp))
			.map(|_| id)
			.expect("TimerWorker should live and be driven as long as TimerApi is alive; qed")
	}
}

/// A `TimerId` wrapper that implements `Ord` and `Eq` using an additional
/// `Timestamp` value.
struct TimerIdWithTimestamp {
	key: Timestamp,
	id: TimerId,
}

impl PartialEq for TimerIdWithTimestamp {
	fn eq(&self, other: &Self) -> bool {
		PartialEq::eq(&self.key, &other.key)
	}
}

impl Eq for TimerIdWithTimestamp {}

impl PartialOrd for TimerIdWithTimestamp {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		PartialOrd::partial_cmp(&self.key, &other.key)
	}
}

impl Ord for TimerIdWithTimestamp {
	fn cmp(&self, other: &Self) -> Ordering {
		Ord::cmp(&self.key, &other.key)
	}
}

pub struct TimerWorker {
	/// Used to broadcast elapsed timers' IDs.
	ready_ids: TracingUnboundedSender<PollableId>,
	/// Used to receive messages from the `TimerApi`.
	from_api: TracingUnboundedReceiver<(TimerId, Timestamp)>,
	/// Timer future driving the wakeups for worker future.
	delay: Option<(Timestamp, Delay)>,
	/// Priority queue for timers, yielding those with earliest timestamps.
	ids: BinaryHeap<Reverse<TimerIdWithTimestamp>>,
}

impl Future for TimerWorker {
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Self::Output> {
		let this = &mut *self;

		// Poll the underlying future to register for a possible future wakeup
		if let Some((_, delay)) = &mut this.delay {
			match Future::poll(Pin::new(delay), cx) {
				Poll::Ready(..) => { this.delay.take(); },
				Poll::Pending => {},
			}
		}

		// Process elapsed timers
		let now = timestamp::now();
		while let Some(true) = this.ids.peek().map(|x| x.0.key <= now) {
			let id = this.ids.pop().expect("We just peeked an element; qed").0.id;

			let _ = this.ready_ids.unbounded_send(id.into());
		}

		// Register the task for a wake-up when we can progress with the earliest timer
		match (this.ids.peek(), this.delay.as_ref()) {
			(Some(Reverse(TimerIdWithTimestamp { key: timestamp, .. })), None) => {
				let diff = timestamp::timestamp_from_now(*timestamp);
				let duration = time::Duration::from_millis(diff.as_millis() as u64);

				this.delay = Some((*timestamp, Delay::new(duration)));
				// Reschedule the task to poll the new underlying timer future
				cx.waker().wake_by_ref();
			},
			_ => {},
		}

		// Check for messages coming from the [`TimerApi`].
		match Stream::poll_next(Pin::new(&mut this.from_api), cx) {
			Poll::Pending => Poll::Pending,
			Poll::Ready(Some((id, timestamp))) => {
				this.ids.push(Reverse(TimerIdWithTimestamp { key: timestamp, id }));

				// Newly added timer may resolve before currently registered
				// earliest one - if that's the case, adjust the new delay.
				match this.delay.as_mut() {
					Some((earliest, delay)) if earliest.diff(&timestamp).millis() > 0 => {
						let diff = timestamp::timestamp_from_now(timestamp);
						let duration = time::Duration::from_millis(diff.as_millis() as u64);

						delay.reset(duration);
					},
					_ => {},
				}
				// Reschedule the task to poll the new underlying timer future
				// (delay could've changed or a fresh, single timer could've been added)
				cx.waker().wake_by_ref();

				Poll::Pending
			},
			// Finished, stop the worker
			Poll::Ready(None) => Poll::Ready(()),
		}
	}
}