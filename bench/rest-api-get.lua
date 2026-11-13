-- GET /users/<id> for a random id among the first 1,000 users: after the
-- warm-up, nearly every read comes from the cache.
math.randomseed(os.time())
request = function()
  return wrk.format("GET", "/users/" .. math.random(1, 1000))
end
