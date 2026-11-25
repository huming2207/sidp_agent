var names = ['pc', 'lr', 'sp'];
const out = {};
for (const n of names) out[n] = await target.readReg(n);
out
