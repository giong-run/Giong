-- One user by id among the first 10,000, on the path in PATH_PREFIX (/pg/ or /my/).
local prefix = os.getenv("PATH_PREFIX") or "/pg/"
math.randomseed(os.time())
request = function()
  return wrk.format("GET", prefix .. math.random(1, 10000))
end
