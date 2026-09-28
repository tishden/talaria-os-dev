const v = globalThis.process ? "" : "";
print("QuickJS on Talaria OS (JavaScript engine in ring 3)");
function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); }
print("fib(28) = " + fib(28));
const a = Array.from({length:10}, (_,i) => (i*7) % 11).sort((x,y)=>x-y);
print("sorted: [" + a.join(",") + "]");
print("map/filter/reduce: " + [1,2,3,4,5,6].filter(x=>x%2).map(x=>x*x).reduce((s,x)=>s+x,0));
print("JSON: " + JSON.stringify({os:"Talaria", lang:"JS", year:2026, ok:true}));
const s = "talaria"; print("string: " + s.toUpperCase() + " len=" + s.length + " rev=" + [...s].reverse().join(""));
print("Math: pi=" + Math.PI.toFixed(5) + " sqrt2=" + Math.sqrt(2).toFixed(5));
print("QuickJS OK on Talaria");
