-- GET /users?limit=20&after=<random>: a page of 20 users from PostgreSQL,
-- never cached.
math.randomseed(os.time())
request = function()
  return wrk.format("GET", "/users?limit=20&after=" .. math.random(0, 9000))
end
