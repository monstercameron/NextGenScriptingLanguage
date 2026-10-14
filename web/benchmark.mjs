// Imported by the browser acceptance harness. Measures the actual deployed worker.
export async function measureWorker(page, examples, fixtures) {
  return page.evaluate(async ({examples,fixtures})=>{
    const worker=new Worker(new URL('./worker.js',location.href),{type:'module'});
    const pending=new Map();let id=0;
    const ready=new Promise((resolve,reject)=>{worker.onmessage=({data})=>{if(data.ready)resolve(data);else if(data.fatal)reject(new Error(data.fatal));else{const waiter=pending.get(data.id);if(waiter){pending.delete(data.id);if(data.error)waiter.reject(new Error(data.error));else waiter.resolve(data);}}};worker.onerror=event=>reject(new Error(event.message));});
    const startup=performance.now();
    const boot=await ready;
    const boot_ms=performance.now()-startup;
    const request=value=>new Promise((resolve,reject)=>{const key=++id;pending.set(key,{resolve,reject});worker.postMessage({id:key,request:value});});
    try {
      for(const [path,content]of Object.entries(fixtures)){const {result}=await request({action:'write',path,content});if(!result.ok)throw new Error(result.error);}
      const results={};
      for(const example of examples.filter(item=>item.id!=='cache')){
        const value={action:'run',source:example.source,stdin:example.stdin??''};
        const run=async()=>{const start=performance.now();const response=await request(value);const roundtrip=performance.now()-start;if(!response.result.ok||response.result.stdout!==example.expected)throw new Error(`benchmark output mismatch: ${example.id}`);return {worker:response.elapsed,roundtrip};};
        const first=await run();
        for(let at=0;at<20;at++)await run();
        const aggregate_start=performance.now();
        const raw=[];for(let at=0;at<200;at++)raw.push(await run());
        const aggregate_elapsed_ms=performance.now()-aggregate_start;
        const stats=key=>{const sorted=raw.map(item=>item[key]).sort((a,b)=>a-b);return {median_ms:(sorted[99]+sorted[100])/2,p95_ms:sorted[189],raw_ms:raw.map(item=>item[key])};};
        results[example.id]={aggregate_elapsed_ms,aggregate_mean_roundtrip_ms:aggregate_elapsed_ms/200,first_request_ms:first,worker:stats('worker'),roundtrip:stats('roundtrip')};
      }
      return {schema:1,status:'passed',recorded_at:new Date().toISOString(),user_agent:navigator.userAgent,wasm_bytes:boot.bytes,worker_boot_ms:boot_ms,samples_per_workflow:200,excluded_warmups:20,timer_note:'performance.now is quantized (commonly 0.1 ms without cross-origin isolation). A zero sample means below timer resolution, not zero cost. Aggregate mean divides wall time around all 200 sequential checked requests by 200.',boundary:'Finite scripts, warm browser WASM worker, virtual Rust applets. Worker includes JSON decode, parse, validation, execution, response serialization and full virtual-file snapshots. Round trip also includes postMessage/structured clone and event scheduling. Boot includes worker startup, cached/local fetch, compilation and instantiation. No OS process spawning and no native/Bash comparison.',results};
    }finally{worker.terminate();}
  },{examples,fixtures});
}
