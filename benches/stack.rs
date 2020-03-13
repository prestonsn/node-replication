// Copyright © 2019 VMware, Inc. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Defines a stack data-structure that can be replicated.
#![allow(unused)]
#![feature(test)]

use std::cell::RefCell;

use node_replication::Dispatch;
use rand::{thread_rng, Rng};

mod mkbench;
mod utils;

use utils::benchmark::*;
use utils::Operation;

/// Operations we can perform on the stack.
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum OpWr {
    /// Add item to stack
    Push(u32),
    /// Pop item from stack
    Pop,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub enum OpRd {}

/// Single-threaded implementation of the stack.
///
/// We just use a vector.
#[derive(Debug, Clone)]
pub struct Stack {
    storage: Vec<u32>,
}

impl Stack {
    pub fn push(&mut self, data: u32) {
        self.storage.push(data);
    }

    pub fn pop(&mut self) -> Option<u32> {
        self.storage.pop()
    }
}

impl Default for Stack {
    /// Return a dummy stack with some initial (50k) elements.
    fn default() -> Stack {
        let mut s = Stack {
            storage: Default::default(),
        };

        for e in 0..50000 {
            s.push(e);
        }

        s
    }
}

impl Dispatch for Stack {
    type ReadOperation = OpRd;
    type WriteOperation = OpWr;
    type Response = Option<u32>;
    type ResponseError = ();

    fn dispatch(&self, _op: Self::ReadOperation) -> Result<Self::Response, Self::ResponseError> {
        unreachable!()
    }

    /// Implements how we execute operations from the log against our local stack
    fn dispatch_mut(
        &mut self,
        op: Self::WriteOperation,
    ) -> Result<Self::Response, Self::ResponseError> {
        match op {
            OpWr::Push(v) => {
                self.push(v);
                return Ok(None);
            }
            OpWr::Pop => return Ok(self.pop()),
        }
    }
}

/// Generate a random sequence of operations that we'll perform:
pub fn generate_operations(nop: usize) -> Vec<Operation<OpRd, OpWr>> {
    let mut orng = thread_rng();
    let mut arng = thread_rng();

    let mut ops = Vec::with_capacity(nop);
    for _i in 0..nop {
        let op: usize = orng.gen();
        match op % 2usize {
            0usize => ops.push(Operation::WriteOperation(OpWr::Pop)),
            1usize => ops.push(Operation::WriteOperation(OpWr::Push(arng.gen()))),
            _ => unreachable!(),
        }
    }

    ops
}

/// Compare against a stack with and without a log in-front.
fn stack_single_threaded(c: &mut TestHarness) {
    env_logger::try_init();

    // Benchmark operations per iteration
    const NOP: usize = 1_000;
    // Log size
    const LOG_SIZE_BYTES: usize = 2 * 1024 * 1024;

    let ops = generate_operations(NOP);
    mkbench::baseline_comparison::<Stack>(c, "stack", ops, LOG_SIZE_BYTES);
}

/// Compare scalability of a node-replicated stack.
fn stack_scale_out(c: &mut TestHarness) {
    mkbench::ScaleBenchBuilder::new()
        .machine_defaults()
        .configure::<Stack>(
            c,
            "stack-scaleout",
            |_cid, rid, _log, replica, _batch_size, rng| {
                let op = rng.gen::<u8>();
                let val = rng.gen::<u32>();

                match op % 2u8 {
                    0u8 => replica.execute(OpWr::Pop, rid).unwrap(),
                    1u8 => replica.execute(OpWr::Push(val), rid).unwrap(),
                    _ => unreachable!(),
                };
            },
        );
}

fn main() {
    let _r = env_logger::try_init();
    let mut harness = Default::default();

    stack_single_threaded(&mut harness);
    stack_scale_out(&mut harness);
}
