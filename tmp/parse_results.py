import re

with open('tmp/v1-tests.log', 'r', errors='replace') as f:
    text = f.read()

pattern = re.compile(r"Running tests\\(\w+)\.rs.*?(?=Running |\Z)", re.DOTALL)
results = []
for m in pattern.finditer(text):
    name = m.group(1)
    block = m.group(0)
    rm = re.search(
        r"test result:\s*(\w+)\.\s*(\d+)\s*passed;\s*(\d+)\s*failed;\s*(\d+)\s*ignored",
        block,
    )
    if rm:
        verdict, p, fl, ig = rm.groups()
        results.append((name, verdict, int(p), int(fl), int(ig)))

for name, verdict, p, fl, ig in results:
    marker = "OK  " if verdict == "ok" else "FAIL"
    print(f"  [{marker}] {name:40s} {p:>4} passed, {fl} failed, {ig} ignored")

print(f"\n--- {len(results)} integration test targets ---")
print(
    f"totals: {sum(r[2] for r in results)} passed, "
    f"{sum(r[3] for r in results)} failed, "
    f"{sum(r[4] for r in results)} ignored"
)
