-- POST /users with an email no other request uses: a tag random to this wrk
-- process, then a counter.
math.randomseed(os.time() + os.clock() * 1000000)
local tag = math.random(1, 2000000000)
local n = 0
wrk.headers["Content-Type"] = "application/json"
request = function()
  n = n + 1
  local body = string.format('{"name":"Bench %d","email":"b%d-%d@example.com"}', n, tag, n)
  return wrk.format("POST", "/users", nil, body)
end
