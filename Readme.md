
![lines of code](https://img.shields.io/tokei/lines/github/worldcoin/signup-sequencer)
[![dependency status](https://deps.rs/repo/github/worldcoin/signup-sequencer/status.svg)](https://deps.rs/repo/github/worldcoin/signup-sequencer)
[![codecov](https://codecov.io/gh/worldcoin/signup-sequencer/branch/main/graph/badge.svg?token=WBPZ9U4TTO)](https://codecov.io/gh/worldcoin/signup-sequencer)
[![CI](https://github.com/worldcoin/signup-sequencer/actions/workflows/test.yml/badge.svg)](https://github.com/worldcoin/signup-sequencer/actions/workflows/test.yml)
[![Audit](https://github.com/worldcoin/signup-sequencer/actions/workflows/audit.yml/badge.svg)](https://github.com/worldcoin/signup-sequencer/actions/workflows/audit.yml)

# Worldcoin Sign-up Sequencer

Sign-up Sequencer does sequencing of data (identities) that are commited in a batch to an Ethereum Smart Contract.

## Table of Contents
1. [Introduction](#introduction)
2. [Getting Started](#getting-started)
3. [Dependencies](#dependencies)
4. [Benchmarking](#benchmarking)
5. [Contributing](#contributing)
6. [Contact](#contact)

## Introduction

Sequencer has 6 API routes.

1. `/insertIdentity` - Accepts identity commitment hash as input which gets added in queue for processing.  
    Identities go trough three tasks.  
    1. Insertion: In the initial stage, the identities are placed into the Sequencer's database.  
    The database is polled every few seconds and added to insertion task.  
    2. Processing: The processing of identities, where current batching tree is taken and processed so we we  
    end up with pre root (the root of tree before proofs are generated), post root, start index and  
    identity commitments (with their proofs). All of those get sent to a [prover](#semaphore-mtb) for proof generation.  
    The identities transaction is then mined, with aforementioned fields and pending identities are sent to task to be mined on-chain.  
    3. Mining:  The transaction ID from processing task gets mined and Sequencer database gets updated accordingly.  
    Now with blockchain and database being in sync, the mined tree gets updated as well.  
2. `/inclusionProof` - Takes the identity commitment hash, and checks for any errors that might have occurred in the insert identity steps.  
    Then leaf index is fetched from the database, corresponding to the identity hash provided, and then the we check if the identity is  
    indeed in the tree. The inclusion proof is then returned to the API caller.  
3. `/verifySemaphoreProof` - This call takes root, signal hash, nullifier hash, external nullifier hash and a proof.  
    The proving key is fetched based on the depth index, and verification key as well.  
    The list of prime fields is created based on request input mentioned before, and then we proceed to verify the proof.   
    Sequencer uses groth16 zk-SNARK implementation.
    The API call returns the proof as response.  
4.  `/addBatchSize` - Adds a prover with specific batch size to a list of provers.  
5.  `/removeBatchSize` - Removes the prover based on batch size.  
6.  `/listBatchSizes` - Lists all provers that are added to the Sequencer.  
     


## Getting Started
Install Protobuf compiler

| Os            | Command                                     |
| ------------- | ------------------------------------------- |
| MacOs         | `brew install protobuf`                     |
| Ubuntu/Debian | `sudo apt-get install -y protobuf-compiler` |

Install [Docker](https://docs.docker.com/get-docker/) - Docker is used to setup the database for testing

Fetch the [postgres](https://hub.docker.com/_/postgres) docker image before running tests.

```shell
docker pull postgres
```

## Database

```shell
docker run --rm -ti -p 5432:5432 -e POSTGRES_PASSWORD=password postgres
```

## Hints

Lint, build, test, run
-
```shell
cargo fmt && cargo clippy --all-targets --features "bench, mimalloc" && cargo build --all-targets --features "bench, mimalloc" && cargo test --all-targets --features "bench, mimalloc" && cargo run --
```

Run benchmarks

```shell
cargo criterion
```
