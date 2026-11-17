//! The everyday commands as methods, the same on [`Client`](crate::Client)
//! and [`Pool`](crate::Pool).

/// Defines the command methods on a type that has
/// `async fn query<T: FromValue>(&self, cmd: &Cmd) -> Result<T, Error>` and
/// `async fn query_raw<T: FromValue>(&self, name: &'static str, args: &[&dyn ToArg]) -> Result<T, Error>`,
/// which sends a command known to be safe to share without building a
/// [`Cmd`](crate::Cmd).
macro_rules! commands {
    () => {
        /// `GET key`: the value of a key, `None` (with `Option<T>`) when it
        /// does not exist.
        ///
        /// ```no_run
        /// # async fn f(client: &giong_redis::Client) -> Result<(), giong_redis::Error> {
        /// let name: Option<String> = client.get("user:7:name").await?;
        /// let raw: Option<Vec<u8>> = client.get("thumbnail:7").await?;
        /// let hits: Option<i64> = client.get("hits").await?;
        /// # Ok(()) }
        /// ```
        ///
        /// Cancel safe: it changes nothing.
        pub async fn get<T: $crate::FromValue>(
            &self,
            key: impl $crate::ToArg,
        ) -> Result<T, $crate::Error> {
            self.query_raw("GET", &[&key]).await
        }

        /// `SET key value`: sets a key, removing any time to live it had.
        ///
        /// Not cancel safe: if the future is dropped, the command may still run
        /// on the server; the connection stays usable.
        pub async fn set(
            &self,
            key: impl $crate::ToArg,
            value: impl $crate::ToArg,
        ) -> Result<(), $crate::Error> {
            self.query_raw("SET", &[&key, &value]).await
        }

        /// `SET key value PX ms`: sets a key that expires after `ttl`
        /// (to the millisecond; 0 is refused by the server).
        ///
        /// Not cancel safe, like [`set`](Self::set).
        pub async fn set_ex(
            &self,
            key: impl $crate::ToArg,
            value: impl $crate::ToArg,
            ttl: ::std::time::Duration,
        ) -> Result<(), $crate::Error> {
            let ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
            self.query_raw("SET", &[&key, &value, &"PX", &ms]).await
        }

        /// `SET key value NX`: sets a key only if it does not exist;
        /// returns whether it was set.
        ///
        /// Not cancel safe: if the future is dropped, the key may have been set
        /// without the caller learning it.
        pub async fn set_nx(
            &self,
            key: impl $crate::ToArg,
            value: impl $crate::ToArg,
        ) -> Result<bool, $crate::Error> {
            let reply: Option<$crate::Value> =
                self.query_raw("SET", &[&key, &value, &"NX"]).await?;
            Ok(reply.is_some())
        }

        /// `DEL key`: removes a key; returns whether it existed.
        ///
        /// Not cancel safe, like [`set`](Self::set).
        pub async fn del(&self, key: impl $crate::ToArg) -> Result<bool, $crate::Error> {
            self.query_raw("DEL", &[&key]).await
        }

        /// `EXISTS key`: whether a key exists.
        ///
        /// Cancel safe: it changes nothing.
        pub async fn exists(&self, key: impl $crate::ToArg) -> Result<bool, $crate::Error> {
            self.query_raw("EXISTS", &[&key]).await
        }

        /// `PEXPIRE key ms`: makes a key expire after `ttl`; returns whether
        /// the key exists.
        ///
        /// Cancel safe: if the future is dropped, the time to live may or may
        /// not have been set, and calling again sets it.
        pub async fn expire(
            &self,
            key: impl $crate::ToArg,
            ttl: ::std::time::Duration,
        ) -> Result<bool, $crate::Error> {
            let ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
            self.query_raw("PEXPIRE", &[&key, &ms]).await
        }

        /// `INCRBY key by`: adds `by` (which may be negative) to a counter,
        /// starting from 0; returns the new value.
        ///
        /// Not cancel safe: if the future is dropped, the counter may have been
        /// changed, and calling again changes it twice.
        pub async fn incr(&self, key: impl $crate::ToArg, by: i64) -> Result<i64, $crate::Error> {
            self.query_raw("INCRBY", &[&key, &by]).await
        }

        /// `MGET key...`: the values of several keys, in order, with `None`
        /// for those that do not exist: read as `Vec<Option<T>>`, as in
        /// `mget::<Vec<Option<String>>>(&["a", "b"])`.
        ///
        /// Cancel safe: it changes nothing.
        pub async fn mget<T: $crate::FromValue>(
            &self,
            keys: &[impl $crate::ToArg],
        ) -> Result<T, $crate::Error> {
            self.query(&$crate::cmd("MGET").args(keys)).await
        }

        /// `HGET key field`: one field of a hash, `None` (with `Option<T>`)
        /// when it or the hash does not exist.
        ///
        /// Cancel safe: it changes nothing.
        pub async fn hget<T: $crate::FromValue>(
            &self,
            key: impl $crate::ToArg,
            field: impl $crate::ToArg,
        ) -> Result<T, $crate::Error> {
            self.query_raw("HGET", &[&key, &field]).await
        }

        /// `HSET key field value`: sets one field of a hash; returns whether
        /// the field is new.
        ///
        /// Not cancel safe, like [`set`](Self::set).
        pub async fn hset(
            &self,
            key: impl $crate::ToArg,
            field: impl $crate::ToArg,
            value: impl $crate::ToArg,
        ) -> Result<bool, $crate::Error> {
            self.query_raw("HSET", &[&key, &field, &value]).await
        }

        /// `HGETALL key`: every field of a hash, read as a
        /// `HashMap<String, T>` (empty when the hash does not exist).
        ///
        /// Cancel safe: it changes nothing.
        pub async fn hgetall<T: $crate::FromValue>(
            &self,
            key: impl $crate::ToArg,
        ) -> Result<T, $crate::Error> {
            self.query_raw("HGETALL", &[&key]).await
        }

        /// `PUBLISH channel message`: returns how many subscribers received
        /// it (on this node).
        ///
        /// Not cancel safe: if the future is dropped, the message may have been
        /// published, and calling again publishes it twice.
        pub async fn publish(
            &self,
            channel: impl $crate::ToArg,
            message: impl $crate::ToArg,
        ) -> Result<u64, $crate::Error> {
            self.query_raw("PUBLISH", &[&channel, &message]).await
        }

        /// `PING`: checks that the server answers.
        ///
        /// Cancel safe: it changes nothing.
        pub async fn ping(&self) -> Result<(), $crate::Error> {
            self.query_raw("PING", &[]).await
        }
    };
}

pub(crate) use commands;
