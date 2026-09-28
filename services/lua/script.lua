print("Lua " .. _VERSION .. " on Talaria OS (ring 3, bare-metal kernel)")
local function fib(n) if n < 2 then return n end return fib(n-1) + fib(n-2) end
print("fib(30) = " .. fib(30))
local t = {}
for i = 1, 12 do t[i] = (i * 7) % 13 end
table.sort(t)
print("sorted: " .. table.concat(t, ","))
local s = "talaria"
print("string: upper=" .. s:upper() .. " len=" .. #s .. " rev=" .. s:reverse())
print(string.format("float: pi=%.5f sqrt2=%.5f 2^20=%d", math.pi, math.sqrt(2), 2^20))
local sum = 0; for i = 1, 1000000 do sum = sum + i end
print("sum(1..1e6) = " .. sum)
print("Lua OK on Talaria")
