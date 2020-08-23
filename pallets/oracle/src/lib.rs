//! # General Oracle Module
//!
//! This module is designed for a general oracle purpose, to make oracle a
//! User-friendly service for those who do not have in-depth knowledge of the
//! offchain worker. With this pallet, you can fetch the offchain data and feed it
//! back onto blockchain and modify some specific runtime storage.
//!
//! ## Overview
//! In this oracle pallet, we can specify a very limited set of storage keys and
//! data providers(also very limited set). Only a permitted account(listed in the
//! provider set) is allowed to feed data onto the chain, through offchain worker.
//!
//! These fetched data will be used to modify the value under the specified storage
//! key after some operations.
//!
//! This module is still a work in progress. It will support more types later.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use frame_support::{
    debug, decl_error, decl_event, decl_module, decl_storage, ensure,
    traits::Get, IterableStorageDoubleMap, IterableStorageMap,
};
use frame_system::{
    ensure_root, ensure_signed,
    offchain::{AppCrypto, CreateSignedTransaction, SendSignedTransaction, Signer},
};
use lite_json::json::{JsonValue, NumberValue};
use sp_core::crypto::KeyTypeId;
use sp_runtime::{
    offchain::{http, Duration},
    FixedPointNumber, FixedU128, SaturatedConversion,
};
use sp_runtime::{
    traits::{StaticLookup, Zero},
    DispatchError, DispatchResult, RuntimeDebug,
};
use sp_std::{
    convert::{TryFrom, TryInto},
    prelude::*,
    str::Chars,
};

#[cfg(test)]
mod tests;

/// The identifier for oracle-specific crypto keys
pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"orcl");

pub mod crypto {
    use super::KEY_TYPE;
    use sp_runtime::
        {app_crypto::{app_crypto, sr25519},
        traits::Verify,
    };
    app_crypto!(sr25519, KEY_TYPE);

    pub type AuthoritySignature = Signature;
    pub type AuthorityId = Public;

    /// For testing
    use sp_core::sr25519::Signature as Sr25519Signature;
    pub struct TestAuthId;
    impl frame_system::offchain::AppCrypto<<Sr25519Signature as Verify>::Signer, Sr25519Signature> for TestAuthId {
        type RuntimeAppPublic = AuthorityId;
        type GenericSignature = sp_core::sr25519::Signature;
        type GenericPublic = sp_core::sr25519::Public;
    }
}

/// The operation type that specifies how we calculate the data
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Encode, Decode, RuntimeDebug)]
pub enum Operations {
    Average,
    Sum,
}

/// A "generic" numeric data type, which can be used in other pallets
/// as a storage value. `lite_json:json::NumberValue` can be converted into
/// PrimitiveOracleType with helper functions.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Encode, Decode, RuntimeDebug)]
pub enum PrimitiveOracleType {
	/// unsigned
    U128(u128),
    FixedU128(FixedU128),
}

impl PrimitiveOracleType {
    pub fn number_type(&self) -> NumberType {
        match self {
            PrimitiveOracleType::U128(_) => NumberType::U128,
            PrimitiveOracleType::FixedU128(_) => NumberType::FixedU128,
        }
    }

    pub fn from_number_value(val: NumberValue, target_type: NumberType) -> Option<Self> {
        match target_type {
            NumberType::U128 => {
                if val.integer < 0 {
                    return None;
                }
                Some(PrimitiveOracleType::U128(val.integer as u128))
            }
            NumberType::FixedU128 => {
                if val.integer < 0 {
                    return None;
                }
                let decimal_point = -(val.fraction_length as i32) + val.exponent;
                let (n, d) = if decimal_point >= 0 {
                    // e.g. 1.35 * 10^3
                    // integer part is (1 * 10^2 + 35) * 10^(-2+3)
                    // decimal part is 1
                    let n = (val.integer * 10_i64.pow(val.fraction_length) + val.fraction as i64)
                        * 10_i64.pow(decimal_point as u32);
                    let d = 1;
                    (n, d)
                } else {
                    // e.g. 1.35 * 10^-2
                    // integer part is (1 * 10^2 + 35)
                    // decimal part is 10^(|-2-2|)
                    let n = val.integer * 10_i64.pow(val.fraction_length) + val.fraction as i64;
                    // let d = decimal_point.abs();
                    let d = 10_i64.pow(decimal_point.abs() as u32);
                    (n, d)
                };
                let res = FixedU128::from((n, d));
                Some(PrimitiveOracleType::FixedU128(res))
            }
        }
    }
}

/// Types of PrimitiveOracleType
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Encode, Decode, RuntimeDebug)]
pub enum NumberType {
    U128,
    FixedU128,
}

impl Default for PrimitiveOracleType {
    fn default() -> Self {
        PrimitiveOracleType::U128(0)
    }
}

type StorageKey = Vec<u8>;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, RuntimeDebug)]
pub struct Info<BlockNumber> {
    /// the key of what we want from the json callback
    key_str: Vec<u8>,
    number_type: NumberType,
    operation: Operations,
    schedule: BlockNumber,
}

impl<BlockNumber> Info<BlockNumber> {
    fn key_str(&self) -> &[u8] {
        &self.key_str
    }

    fn number_type(&self) -> NumberType {
        self.number_type
    }
}

const RING_BUF_LEN: usize = 8;

use frame_support::traits::{Contains, EnsureOrigin};
use sp_runtime::traits::{CheckedDiv, Saturating};
use sp_std::{marker::PhantomData, prelude::*};

pub struct DataFeedGet<Key: Get<[u8; 32]>, Value: Get<PrimitiveOracleType>>(
    PhantomData<Key>,
    PhantomData<Value>,
);

impl<Key: Get<[u8; 32]>, Value: Get<PrimitiveOracleType>> Get<PrimitiveOracleType>
    for DataFeedGet<Key, Value>
{
    fn get() -> PrimitiveOracleType {
        frame_support::storage::unhashed::get_or(Key::get().as_ref(), Value::get())
    }
}

pub trait Trait: CreateSignedTransaction<Call<Self>> {
    /// The identifier type for an offchain worker.
    type AuthorityId: AppCrypto<Self::Public, Self::Signature>;
    /// Because this pallet emits events, it depends on the runtime's definition of an event.
    type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;
    /// The overarching dispatch call type.
    type Call: From<Call<Self>>;
    /// The origin that can schedule an update
    type DispatchOrigin: EnsureOrigin<Self::Origin, Success = Self::AccountId>;
}

decl_event!(
    pub enum Event<T>
    where
        AccountId = <T as frame_system::Trait>::AccountId,
    {   /// New data stored on blokchain
        NewData(AccountId, StorageKey, PrimitiveOracleType),
    }
);

/// Errors inform users that something went wrong.
decl_error! {
    pub enum Error for Module<T: Trait> {
        /// If the storage key already registered
        ExistedKey,
        /// You can not put data under the key that is not registered
        InvalidKey,
        /// If the data type does not match the one specified in Infos
        InvalidValue,
        /// user is not allowed to feed data onto the blockchain
        NotAllowed,
    }
}

decl_storage! {
    // A unique name is used to ensure that the pallet's storage items are isolated.
    // This name may be updated, but each pallet in the runtime must use a unique name.
    // ---------------------------------vvvvvvvvvvvvvv
    trait Store for Module<T: Trait> as Oracle {
        /// A set of storage keys
        /// A storage key in this set refers to a storage value in other pallets,
        pub ActiveParamTypes get(fn all_keys): Vec<StorageKey>;

        /// A set of accounts
        /// Only those in this set are allowed to feed data onto the chain
        pub ActiveProviders get(fn all_providers): Vec<T::AccountId>;

        ///
        pub Infos get(fn infos): map hasher(twox_64_concat) StorageKey => Option<Info<T::BlockNumber>>;

        /// permissioned URL that could be used to fetch data
        pub Url get(fn url): map hasher(twox_64_concat) StorageKey => Option<Vec<u8>>;

        pub DataFeeds get(fn feeded_data): double_map hasher(blake2_128_concat) StorageKey,
            hasher(blake2_128_concat) T::AccountId => Option<[PrimitiveOracleType; RING_BUF_LEN]>;
    }
}

impl<T: Trait> Contains<T::AccountId> for Module<T> {
    fn sorted_members() -> Vec<T::AccountId> {
        Self::all_providers()
    }
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        // Events must be initialized if they are used by the pallet.
        fn deposit_event() = default;

        /// Register a storage key under which the value can be modified
        /// by this oracle pallet with some rules
        #[weight = 0]
        pub fn register_storage_key(origin, key: StorageKey, info: Info<T::BlockNumber>) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            // parse origin
            ActiveParamTypes::try_mutate::<_, DispatchError, _>(|v| {
                if v.contains(&key) {
                    return Err(Error::<T>::ExistedKey)?;
                }
                v.push(key.clone());
                Ok(())
            })?;
            Infos::<T>::insert(key, info);
            Ok(())
        }

        /// remove the storage key from the limited set so the oracle pallet no longer
        /// can change the corresponding value afterwards.
        #[weight = 0]
        pub fn remove_storage_key(origin, key: StorageKey) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            ActiveParamTypes::mutate(|v| {
                // remove key
                v.retain(|k| k != &key);
            });
            Infos::<T>::remove(&key);
            DataFeeds::<T>::remove_prefix(&key);
            Ok(())
        }

        #[weight = 0]
        pub fn set_url(origin, key: StorageKey, url: Vec<u8>) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            Url::insert(&key, url);
            Ok(())
        }

        /// Submit a new data under the specific storage key.
        #[weight = 0]
        pub fn feed_data(origin, key: StorageKey, value: PrimitiveOracleType) -> DispatchResult {
            let who = ensure_signed(origin)?;
            Self::feed_data_impl(who, key, value)
        }

        #[weight = 0]
        pub fn add_provider(origin, new_one: <T::Lookup as StaticLookup>::Source) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            let new_one = T::Lookup::lookup(new_one)?;

            ActiveProviders::<T>::mutate(|v| {
                if !v.contains(&new_one) {
                    v.push(new_one);
                }
            });
            Ok(())
        }

        #[weight = 0]
        pub fn remove_provider(origin, who: <T::Lookup as StaticLookup>::Source) -> DispatchResult {
            T::DispatchOrigin::try_origin(origin).map(|_| ()).or_else(ensure_root)?;
            let who = T::Lookup::lookup(who)?;

            ActiveProviders::<T>::mutate(|v| {
                v.retain(|accountid| accountid != &who);
            });
            Ok(())
        }

        fn on_finalize(_n: T::BlockNumber) {
            for (key, info) in Infos::<T>::iter() {
                if  _n % info.schedule == Zero::zero() {
                    Self::calc(key, info);
                }
            }
        }

        fn offchain_worker(block_number: T::BlockNumber) {
            debug::native::info!("Initialize offchain workers!");

            for key in Self::all_keys() {
                let res = Self::fetch_and_send_signed(&key);
                if let Err(e) = res {
                    debug::error!("Error: {}", e);
                }
            }

            // TODO: set conditions to calc
        }
    }
}
impl<T: Trait> Module<T> {
    fn feed_data_impl(
        who: T::AccountId,
        key: StorageKey,
        value: PrimitiveOracleType,
    ) -> DispatchResult {
        if !Self::all_keys().contains(&key) {
            Err(Error::<T>::InvalidKey)?
        }
        let info: Info<_> = Self::infos(&key).ok_or(Error::<T>::InvalidKey)?;
        if value.number_type() != info.number_type {
            Err(Error::<T>::InvalidValue)?
        }
        debug::info!("Feeding data to {:?}", key);
        DataFeeds::<T>::try_mutate_exists(key.clone(), who.clone(), |data| {
            match data {
                Some(data) => {
                    let mut new_data = [Default::default(); RING_BUF_LEN];
                    new_data[0] = value;
                    // move data buf to new_data buf, and drop last item
                    new_data[1..].copy_from_slice(&data[0..(data.len() - 2)]);
                    *data = new_data;
                }
                None => {
                    *data = Some([value; RING_BUF_LEN]);
                }
            }
            Self::deposit_event(RawEvent::NewData(who, key, value));
            Ok(())
        })
    }

    fn fetch_and_send_signed(storage_key: &StorageKey) -> Result<(), &'static str> {
        let signer = Signer::<T, T::AuthorityId>::all_accounts();
        if !signer.can_sign() {
            return Err(
                "No local accounts available. Consider adding one via `author_insertKey` RPC.",
            )?;
        }
        let data = Self::fetch_data(storage_key)?;

        let results =
            signer.send_signed_transaction(|_account| Call::feed_data(storage_key.to_vec(), data));

        for (acc, res) in &results {
            match res {
                Ok(()) => debug::info!("[{:?}] Submitted data: {:?}", acc.id, data),
                Err(e) => debug::error!("[{:?}] Failed to submit transaction: {:?}", acc.id, e),
            }
        }

        Ok(())
    }

    fn calc_impl<N: Saturating + CheckedDiv + Copy, F: FnOnce(usize) -> N>(
        numbers: &[N],
        zero: N,
        convert: F,
        op: Operations,
    ) -> N {
        match op {
            Operations::Sum => {
                let sum = numbers.iter().fold(zero, |a, b| a.saturating_add(*b));
                sum
            }
            Operations::Average => {
                if numbers.is_empty() {
                    zero
                } else {
                    let sum = numbers.iter().fold(zero, |a, b| a.saturating_add(*b));
                    // let n = N::from(numbers.len() as u128);
                    let n = convert(numbers.len());
                    sum.checked_div(&n).unwrap_or(zero)
                }
            }
        }
    }

    fn calc(key: StorageKey, info: Info<T::BlockNumber>) {
        // calc result would drain all old data
        let data: Vec<PrimitiveOracleType> =
            DataFeeds::<T>::drain_prefix(&key).fold(Vec::new(), |mut src, (who, datas)| {
                if Self::all_providers().contains(&who) {
                    src.extend(&datas);
                }
                src
            });

        let result = match info.number_type {
            NumberType::U128 => {
                let numbers = data
                    .into_iter()
                    .filter_map(|num: PrimitiveOracleType| match num {
                        PrimitiveOracleType::U128(a) => Some(a),
                        PrimitiveOracleType::FixedU128(_) => None,
                    })
                    .collect::<Vec<u128>>();

                let res: u128 =
                    Self::calc_impl(&numbers, 0_u128, |len| len as u128, info.operation);
                // PrimitiveOracleType::from_u32_value(res, info.number_type)
                PrimitiveOracleType::U128(res)
            }
            NumberType::FixedU128 => {
                let numbers = data
                    .into_iter()
                    .filter_map(|num: PrimitiveOracleType| match num {
                        PrimitiveOracleType::U128(_) => None,
                        PrimitiveOracleType::FixedU128(a) => Some(a),
                    })
                    .collect::<Vec<FixedU128>>();

                let res = Self::calc_impl(
                    &numbers,
                    FixedU128::zero(),
                    |len| FixedU128::from(len as u128),
                    info.operation,
                );
                PrimitiveOracleType::FixedU128(res)
            }
        };

        Self::set_storage_value(key, result);
    }

    fn set_storage_value(key: Vec<u8>, value: PrimitiveOracleType) {
        frame_support::storage::unhashed::put(&key, &value);
    }

    fn fetch_data(storage_key: &StorageKey) -> Result<PrimitiveOracleType, &'static str> {
        // We want to keep the offchain worker execution time reasonable, so we set a hard-coded
        // deadline to 2s to complete the external call.
        // You can also wait idefinitely for the response, however you may still get a timeout
        // coming from the host machine.
        let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(2_000));
        // TODO: if error
        let url = Self::url(storage_key).ok_or("no url for this storage key")?;

        let request =
            http::Request::get(core::str::from_utf8(&url).map_err(|_| "parse url error")?);

        // to alter request headers or stream body content in case of non-GET requests.
        let pending = request
            .deadline(deadline)
            .send()
            .map_err(|_| "Request had timed out.")?;

        let response = pending
            .try_wait(deadline)
            .map_err(|_| "Deadline has been reached")?
            .map_err(|_| "http error")?;
        // Let's check the status code before we proceed to reading the response.
        if response.code != 200 {
            debug::warn!("Unexpected status code: {}", response.code);
            Err("Unknown error has been encountered.")?;
        }

        let body = response.body().collect::<Vec<u8>>();
        // TODO: handle error here
        let info = Self::infos(&storage_key).ok_or("storage key not exist")?;
        let number_type = info.number_type();

        // Create a str slice from the body.
        let body_str = sp_std::str::from_utf8(&body).map_err(|_| {
            debug::warn!("No UTF8 body");
            "Unknown error has been encountered."
        })?;
        // TODO: handle error
        let key_str = core::str::from_utf8(info.key_str())
            .map_err(|_| "json key is invalid")?
            .chars();
        let data = match Self::parse_data(key_str, number_type, body_str) {
            Some(data) => Ok(data),
            None => {
                debug::warn!("Unable to extract price from the response: {:?}", body_str);
                Err("Unknown error has been encountered.")
            }
        }?;
        //		debug::warn!("Got Data: {}", data);
        Ok(data)
    }

    fn parse_data(
        key_str: Chars,
        number_type: NumberType,
        data: &str,
    ) -> Option<PrimitiveOracleType> {
        let mut key_str = key_str;
        let val = lite_json::parse_json(data);
        let output = val.ok().and_then(|v| match v {
            JsonValue::Object(obj) => obj
                .into_iter()
                .find(|(k, _)| k.iter().all(|k| Some(*k) == key_str.next()))
                .and_then(|v| match v.1 {
                    JsonValue::Number(number) => Some(number),
                    _ => None,
                }),
            _ => None,
        })?;

        PrimitiveOracleType::from_number_value(output, number_type)
    }
}
