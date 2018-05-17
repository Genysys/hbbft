use fmt::{HexBytes, HexList, HexProof};
use merkle::{MerkleTree, Proof};
use reed_solomon_erasure as rse;
use reed_solomon_erasure::ReedSolomon;
use ring::digest;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::{self, Debug};
use std::iter;

use messaging::{DistAlgorithm, Target, TargetedMessage};

/// The three kinds of message sent during the reliable broadcast stage of the
/// consensus algorithm.
#[cfg_attr(feature = "serialization-serde", derive(Serialize, Deserialize))]
#[derive(Clone, PartialEq)]
pub enum BroadcastMessage {
    Value(Proof<Vec<u8>>),
    Echo(Proof<Vec<u8>>),
    Ready(Vec<u8>),
}

impl Debug for BroadcastMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            BroadcastMessage::Value(ref v) => write!(f, "Value({:?})", HexProof(&v)),
            BroadcastMessage::Echo(ref v) => write!(f, "Echo({:?})", HexProof(&v)),
            BroadcastMessage::Ready(ref bytes) => write!(f, "Ready({:?})", HexBytes(bytes)),
        }
    }
}

/// Reliable Broadcast algorithm instance.
///
/// The Reliable Broadcast Protocol assumes a network of `N` nodes that send signed messages to
/// each other, with at most `f` of them malicious, where `3 * f < N`. Handling the networking and
/// signing is the responsibility of this crate's user: only when a message has been verified to be
/// "from node i", it can be handed to the `Broadcast` instance. One of the nodes is the "proposer"
/// who sends a value. Under the above conditions, the protocol guarantees that either all or none
/// of the good nodes output a value, and that if the proposer is good, all good nodes output the
/// proposed value.
///
/// The algorithm works as follows:
///
/// * The proposer uses a Reed-Solomon code to split the value into `N` chunks, `f + 1` of which
/// suffice to reconstruct the value. These chunks are put into a Merkle tree, so that with the
/// tree's root hash `h`, branch `bi` and chunk `si`, the `i`-th chunk `si` can be verified by
/// anyone to belong to the Merkle tree with root hash `h`. These values are "proof" number `i`:
/// `pi`.
/// * The proposer sends `Value(pi)` to node `i`. It translates to: "I am the proposer, and `pi`
/// contains the `i`-th share of my value."
/// * Every (good) node that receives `Value(pi)` from the proposer sends it on to everyone else as
/// `Echo(pi)`. An `Echo` translates to: "I have received `pi` directly from the proposer." If the
/// proposer sends another `Value` message, that is ignored.
/// * So every node that has received at least `f + 1` `Echo` messages with the same root
/// hash will be able to decode a value.
/// * Every node that has received `N - f` `Echo`s with the same root hash from different nodes
/// knows that at least `f + 1` _good_ nodes have sent an `Echo` with that hash to everyone, and
/// therefore everyone will eventually receive at least `f + 1` of them. So upon receiving `N - f`
/// `Echo`s, they send a `Ready(h)` to everyone to indicate that. `Ready` translates to: "I know
/// that everyone will eventually be able to decode the value." Moreover, since every good node
/// only ever sends one kind of `Echo` message, this cannot happen for two different root hashes.
/// * Even without enough `Echo` messages, if a node receives `f + 1` `Ready` messages, it knows
/// that at least one _good_ node has sent `Ready`. It therefore also knows that everyone will be
/// able to decode eventually, and multicasts `Ready` itself.
/// * If a node has received `2 * f + 1` `Ready`s (with matching root hash) from different nodes,
/// it knows that at least `f + 1` _good_ nodes have sent it. Therefore, every good node will
/// eventually receive `f + 1`, and multicast it itself. Therefore, every good node will eventually
/// receive `2 * f + 1` `Ready`s, too. _And_ we know at this point that every good node will
/// eventually be able to decode (i.e. receive at least `f + 1` `Echo` messages).
/// * So a node with `2 * f + 1` `Ready`s and `f + 1` `Echos` will decode and _output_ the value,
/// knowing that every other good node will eventually do the same.
pub struct Broadcast<N> {
    /// The UID of this node.
    our_id: N,
    /// The UID of the sending node.
    proposer_id: N,
    /// UIDs of all nodes for iteration purposes.
    all_uids: BTreeSet<N>,
    num_nodes: usize,
    num_faulty_nodes: usize,
    data_shard_num: usize,
    coding: Coding,
    /// Whether we have already multicast `Echo`.
    echo_sent: bool,
    /// Whether we have already multicast `Ready`.
    ready_sent: bool,
    /// Whether we have already output a value.
    decided: bool,
    /// The proofs we have received via `Echo` messages, by sender ID.
    echos: BTreeMap<N, Proof<Vec<u8>>>,
    /// The root hashes we received via `Ready` messages, by sender ID.
    readys: BTreeMap<N, Vec<u8>>,
    /// The outgoing message queue.
    messages: VecDeque<TargetedMessage<BroadcastMessage, N>>,
    /// The output, if any.
    output: Option<Vec<u8>>,
}

impl<N: Eq + Debug + Clone + Ord> DistAlgorithm for Broadcast<N> {
    type NodeUid = N;
    // TODO: Allow anything serializable and deserializable, i.e. make this a type parameter
    // T: Serialize + DeserializeOwned
    type Input = Vec<u8>;
    type Output = Self::Input;
    type Message = BroadcastMessage;
    type Error = Error;

    fn input(&mut self, input: Self::Input) -> Result<(), Self::Error> {
        if self.our_id != self.proposer_id {
            return Err(Error::InstanceCannotPropose);
        }
        // Split the value into chunks/shards, encode them with erasure codes.
        // Assemble a Merkle tree from data and parity shards. Take all proofs
        // from this tree and send them, each to its own node.
        let proof = self.send_shards(input)?;
        // TODO: We'd actually need to return the output here, if it was only one node. Should that
        // use-case be supported?
        let our_id = self.our_id.clone();
        self.handle_value(&our_id, proof)
    }

    fn handle_message(&mut self, sender_id: &N, message: Self::Message) -> Result<(), Self::Error> {
        if !self.all_uids.contains(sender_id) {
            return Err(Error::UnknownSender);
        }
        match message {
            BroadcastMessage::Value(p) => self.handle_value(sender_id, p),
            BroadcastMessage::Echo(p) => self.handle_echo(sender_id, p),
            BroadcastMessage::Ready(ref hash) => self.handle_ready(sender_id, hash),
        }
    }

    fn next_message(&mut self) -> Option<TargetedMessage<Self::Message, N>> {
        self.messages.pop_front()
    }

    fn next_output(&mut self) -> Option<Self::Output> {
        self.output.take()
    }

    fn terminated(&self) -> bool {
        self.decided
    }

    fn our_id(&self) -> &N {
        &self.our_id
    }
}

impl<N: Eq + Debug + Clone + Ord> Broadcast<N> {
    /// Creates a new broadcast instance to be used by node `our_id` which expects a value proposal
    /// from node `proposer_id`.
    pub fn new(our_id: N, proposer_id: N, all_uids: BTreeSet<N>) -> Result<Self, Error> {
        let num_nodes = all_uids.len();
        let num_faulty_nodes = (num_nodes - 1) / 3;
        let parity_shard_num = 2 * num_faulty_nodes;
        let data_shard_num = num_nodes - parity_shard_num;
        let coding = Coding::new(data_shard_num, parity_shard_num)?;

        Ok(Broadcast {
            our_id,
            proposer_id,
            all_uids,
            num_nodes,
            num_faulty_nodes,
            data_shard_num,
            coding,
            echo_sent: false,
            ready_sent: false,
            decided: false,
            echos: BTreeMap::new(),
            readys: BTreeMap::new(),
            messages: VecDeque::new(),
            output: None,
        })
    }

    /// Breaks the input value into shards of equal length and encodes them --
    /// and some extra parity shards -- with a Reed-Solomon erasure coding
    /// scheme. The returned value contains the shard assigned to this
    /// node. That shard doesn't need to be sent anywhere. It gets recorded in
    /// the broadcast instance.
    fn send_shards(&mut self, mut value: Vec<u8>) -> Result<Proof<Vec<u8>>, Error> {
        let data_shard_num = self.coding.data_shard_count();
        let parity_shard_num = self.coding.parity_shard_count();

        debug!(
            "Data shards: {}, parity shards: {}",
            self.data_shard_num, parity_shard_num
        );
        // Insert the length of `v` so it can be decoded without the padding.
        let payload_len = value.len() as u8;
        value.insert(0, payload_len); // TODO: Handle messages larger than 255 bytes.
        let value_len = value.len();
        // Size of a Merkle tree leaf value, in bytes.
        let shard_len = if value_len % data_shard_num > 0 {
            value_len / data_shard_num + 1
        } else {
            value_len / data_shard_num
        };
        // Pad the last data shard with zeros. Fill the parity shards with
        // zeros.
        value.resize(shard_len * (data_shard_num + parity_shard_num), 0);

        debug!("value_len {}, shard_len {}", value_len, shard_len);

        // Divide the vector into chunks/shards.
        let shards_iter = value.chunks_mut(shard_len);
        // Convert the iterator over slices into a vector of slices.
        let mut shards: Vec<&mut [u8]> = shards_iter.collect();

        debug!("Shards before encoding: {:?}", HexList(&shards));

        // Construct the parity chunks/shards
        self.coding.encode(&mut shards)?;

        debug!("Shards: {:?}", HexList(&shards));

        // TODO: `MerkleTree` generates the wrong proof if a leaf occurs more than once, so we
        // prepend an "index byte" to each shard. Consider using the `merkle_light` crate instead.
        let shards_t: Vec<Vec<u8>> = shards
            .into_iter()
            .enumerate()
            .map(|(i, s)| iter::once(i as u8).chain(s.iter().cloned()).collect())
            .collect();

        // Convert the Merkle tree into a partial binary tree for later
        // deconstruction into compound branches.
        let mtree = MerkleTree::from_vec(&digest::SHA256, shards_t);

        // Default result in case of `gen_proof` error.
        let mut result = Err(Error::ProofConstructionFailed);
        assert_eq!(self.num_nodes, mtree.iter().count());

        // Send each proof to a node.
        for (leaf_value, uid) in mtree.iter().zip(&self.all_uids) {
            let proof = mtree
                .gen_proof(leaf_value.to_vec())
                .ok_or(Error::ProofConstructionFailed)?;
            if *uid == self.our_id {
                // The proof is addressed to this node.
                result = Ok(proof);
            } else {
                // Rest of the proofs are sent to remote nodes.
                let msg = Target::Node(uid.clone()).message(BroadcastMessage::Value(proof));
                self.messages.push_back(msg);
            }
        }

        result
    }

    /// Handles a received echo and verifies the proof it contains.
    fn handle_value(&mut self, sender_id: &N, p: Proof<Vec<u8>>) -> Result<(), Error> {
        // If the sender is not the proposer, this is not the first `Value` or the proof is invalid,
        // ignore.
        if *sender_id != self.proposer_id {
            info!(
                "Node {:?} received Value from {:?} instead of {:?}.",
                self.our_id, sender_id, self.proposer_id
            );
            return Ok(());
        }
        if self.echo_sent {
            info!("Node {:?} received multiple Values.", self.our_id);
            return Ok(());
        }
        if !self.validate_proof(&p, &self.our_id) {
            return Ok(());
        }

        // Otherwise multicast the proof in an `Echo` message, and handle it ourselves.
        self.echo_sent = true;
        let our_id = self.our_id.clone();
        self.handle_echo(&our_id, p.clone())?;
        let echo_msg = Target::All.message(BroadcastMessage::Echo(p));
        self.messages.push_back(echo_msg);
        Ok(())
    }

    /// Handles a received `Echo` message.
    fn handle_echo(&mut self, sender_id: &N, p: Proof<Vec<u8>>) -> Result<(), Error> {
        // If the proof is invalid or the sender has already sent `Echo`, ignore.
        if self.echos.contains_key(sender_id) {
            info!(
                "Node {:?} received multiple Echos from {:?}.",
                self.our_id, sender_id,
            );
            return Ok(());
        }
        if !self.validate_proof(&p, sender_id) {
            return Ok(());
        }

        let hash = p.root_hash.clone();

        // Save the proof for reconstructing the tree later.
        self.echos.insert(sender_id.clone(), p);

        if self.ready_sent || self.count_echos(&hash) < self.num_nodes - self.num_faulty_nodes {
            return self.compute_output(&hash);
        }

        // Upon receiving `N - f` `Echo`s with this root hash, multicast `Ready`.
        self.ready_sent = true;
        let ready_msg = Target::All.message(BroadcastMessage::Ready(hash.clone()));
        self.messages.push_back(ready_msg);
        let our_id = self.our_id.clone();
        self.handle_ready(&our_id, &hash)
    }

    /// Handles a received `Ready` message.
    fn handle_ready(&mut self, sender_id: &N, hash: &[u8]) -> Result<(), Error> {
        // If the sender has already sent a `Ready` before, ignore.
        if self.readys.contains_key(sender_id) {
            info!(
                "Node {:?} received multiple Readys from {:?}.",
                self.our_id, sender_id
            );
            return Ok(());
        }

        self.readys.insert(sender_id.clone(), hash.to_vec());

        // Upon receiving f + 1 matching Ready(h) messages, if Ready
        // has not yet been sent, multicast Ready(h).
        if self.count_readys(hash) == self.num_faulty_nodes + 1 && !self.ready_sent {
            // Enqueue a broadcast of a Ready message.
            self.ready_sent = true;
            let ready_msg = Target::All.message(BroadcastMessage::Ready(hash.to_vec()));
            self.messages.push_back(ready_msg);
        }
        self.compute_output(&hash)
    }

    /// Checks whether the condition for output are met for this hash, and if so, sets the output
    /// value.
    fn compute_output(&mut self, hash: &[u8]) -> Result<(), Error> {
        if self.decided || self.count_readys(hash) <= 2 * self.num_faulty_nodes
            || self.count_echos(hash) <= self.num_faulty_nodes
        {
            return Ok(());
        }

        // Upon receiving 2f + 1 matching Ready(h) messages, wait for N − 2f Echo messages.
        self.decided = true;
        let mut leaf_values: Vec<Option<Box<[u8]>>> = self.all_uids
            .iter()
            .map(|id| {
                self.echos.get(id).and_then(|p| {
                    if p.root_hash.as_slice() == hash {
                        Some(p.value.clone().into_boxed_slice())
                    } else {
                        None
                    }
                })
            })
            .collect();
        let value = decode_from_shards(&mut leaf_values, &self.coding, self.data_shard_num, hash)?;
        self.output = Some(value);
        Ok(())
    }

    /// Returns `i` if `node_id` is the `i`-th ID among all participating nodes.
    fn index_of_node(&self, node_id: &N) -> Option<usize> {
        self.all_uids.iter().position(|id| id == node_id)
    }

    /// Returns `true` if the proof is valid and has the same index as the node ID. Otherwise
    /// logs an info message.
    fn validate_proof(&self, p: &Proof<Vec<u8>>, id: &N) -> bool {
        if !p.validate(&p.root_hash) {
            info!(
                "Node {:?} received invalid proof: {:?}",
                self.our_id,
                HexProof(&p)
            );
            false
        } else if self.index_of_node(id) != Some(p.value[0] as usize)
            || p.index(self.num_nodes) != p.value[0] as usize
        {
            info!(
                "Node {:?} received proof for wrong position: {:?}.",
                self.our_id,
                HexProof(&p)
            );
            false
        } else {
            true
        }
    }

    /// Returns the number of nodes that have sent us an `Echo` message with this hash.
    fn count_echos(&self, hash: &[u8]) -> usize {
        self.echos
            .values()
            .filter(|p| p.root_hash.as_slice() == hash)
            .count()
    }

    /// Returns the number of nodes that have sent us a `Ready` message with this hash.
    fn count_readys(&self, hash: &[u8]) -> usize {
        self.readys
            .values()
            .filter(|h| h.as_slice() == hash)
            .count()
    }
}

/// A wrapper for `ReedSolomon` that doesn't panic if there are no parity shards.
enum Coding {
    /// A `ReedSolomon` instance with at least one parity shard.
    ReedSolomon(Box<ReedSolomon>),
    /// A no-op replacement that doesn't encode or decode anything.
    Trivial(usize),
}

impl Coding {
    /// Creates a new `Coding` instance with the given number of shards.
    fn new(data_shard_num: usize, parity_shard_num: usize) -> Result<Self, Error> {
        Ok(if parity_shard_num > 0 {
            let rs = ReedSolomon::new(data_shard_num, parity_shard_num)?;
            Coding::ReedSolomon(Box::new(rs))
        } else {
            Coding::Trivial(data_shard_num)
        })
    }

    /// Returns the number of data shards.
    fn data_shard_count(&self) -> usize {
        match *self {
            Coding::ReedSolomon(ref rs) => rs.data_shard_count(),
            Coding::Trivial(dsc) => dsc,
        }
    }

    /// Returns the number of parity shards.
    fn parity_shard_count(&self) -> usize {
        match *self {
            Coding::ReedSolomon(ref rs) => rs.parity_shard_count(),
            Coding::Trivial(_) => 0,
        }
    }

    /// Constructs (and overwrites) the parity shards.
    fn encode(&self, slices: &mut [&mut [u8]]) -> Result<(), Error> {
        match *self {
            Coding::ReedSolomon(ref rs) => rs.encode(slices)?,
            Coding::Trivial(_) => (),
        }
        Ok(())
    }

    /// If enough shards are present, reconstructs the missing ones.
    fn reconstruct_shards(&self, shards: &mut [Option<Box<[u8]>>]) -> Result<(), Error> {
        match *self {
            Coding::ReedSolomon(ref rs) => rs.reconstruct_shards(shards)?,
            Coding::Trivial(_) => {
                if shards.iter().any(Option::is_none) {
                    return Err(rse::Error::TooFewShardsPresent.into());
                }
            }
        }
        Ok(())
    }
}

/// Errors returned by the broadcast instance.
#[derive(Debug, Clone)]
pub enum Error {
    RootHashMismatch,
    Threading,
    ProofConstructionFailed,
    ReedSolomon(rse::Error),
    InstanceCannotPropose,
    NotImplemented,
    UnknownSender,
}

impl From<rse::Error> for Error {
    fn from(err: rse::Error) -> Error {
        Error::ReedSolomon(err)
    }
}

fn decode_from_shards<T>(
    leaf_values: &mut [Option<Box<[u8]>>],
    coding: &Coding,
    data_shard_num: usize,
    root_hash: &[u8],
) -> Result<T, Error>
where
    T: From<Vec<u8>>,
{
    // Try to interpolate the Merkle tree using the Reed-Solomon erasure coding scheme.
    coding.reconstruct_shards(leaf_values)?;

    // Recompute the Merkle tree root.

    // Collect shards for tree construction.
    let shards: Vec<Vec<u8>> = leaf_values
        .iter()
        .filter_map(|l| l.as_ref().map(|v| v.to_vec()))
        .collect();

    debug!("Reconstructed shards: {:?}", HexList(&shards));

    // Construct the Merkle tree.
    let mtree = MerkleTree::from_vec(&digest::SHA256, shards);
    // If the root hash of the reconstructed tree does not match the one
    // received with proofs then abort.
    if &mtree.root_hash()[..] != root_hash {
        // NOTE: The paper does not define the meaning of *abort*. But it is
        // sensible not to continue trying to reconstruct the tree after this
        // point. This instance must have received incorrect shards.
        Err(Error::RootHashMismatch)
    } else {
        // Reconstruct the value from the data shards.
        Ok(glue_shards(mtree, data_shard_num))
    }
}

/// Concatenates the first `n` leaf values of a Merkle tree `m` in one value of
/// type `T`. This is useful for reconstructing the data value held in the tree
/// and forgetting the leaves that contain parity information.
fn glue_shards<T>(m: MerkleTree<Vec<u8>>, n: usize) -> T
where
    T: From<Vec<u8>>,
{
    let t: Vec<u8> = m.into_iter()
        .take(n)
        .flat_map(|s| s.into_iter().skip(1)) // Drop the index byte.
        .collect();
    let payload_len = t[0] as usize;
    debug!("Glued data shards {:?}", HexBytes(&t[1..(payload_len + 1)]));

    t[1..(payload_len + 1)].to_vec().into()
}
