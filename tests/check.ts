const token=(await Bun.file('/home/bun/.opencodex/service-api-token').text()).trim(); // Disposable test-volume token only.
const base=process.env.TEST_BASE ?? 'http://127.0.0.1:10100';
const headers={Authorization:`Bearer ${token}`,'Content-Type':'application/json'};
const catalog=await fetch(base+'/v1/models',{headers});
if(catalog.status!==200) throw Error(`catalog HTTP ${catalog.status}`);
const models=await catalog.json() as {data:{id:string}[]};
const model=models.data.find(m=>m.id.includes('gateway-test'))?.id;
if(!model) throw Error('fake model missing from catalog');
console.log('authenticated_catalog=PASS');
for(const stream of [false,true]) {
 const r=await fetch(base+'/v1/responses',{method:'POST',headers,body:JSON.stringify({model,input:'ping',stream})});
 const body=await r.text();
 if(!r.ok || !body.includes('gateway-pong')) throw Error(`response stream=${stream} HTTP ${r.status}: ${body.slice(0,400)}`);
 if(stream && (!body.includes('response.output_text.delta') || !body.includes('response.completed'))) throw Error('incomplete SSE');
 console.log(`${stream?'streaming':'nonstreaming'}=PASS`);
}
