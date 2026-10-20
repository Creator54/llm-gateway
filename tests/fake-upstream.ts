const response = (text: string) => ({ id: 'resp_gateway_test', object: 'response', created_at: 1, status: 'completed', model: 'gateway-test', output: [{ id: 'msg_test', type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text, annotations: [] }] }], usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 } });
Bun.serve({port: 10302, hostname:'0.0.0.0', async fetch(req) {
 const path = new URL(req.url).pathname;
 if(path.endsWith('/models')) return Response.json({object:'list',data:[{id:'gateway-test',object:'model',created:1,owned_by:'test'}]});
 if(!path.endsWith('/responses')) return new Response('not found',{status:404});
 const body = await req.json() as {stream?:boolean};
 if(!body.stream) return Response.json(response('gateway-pong'));
 const done=response('gateway-pong');
 const events=[['response.created',{response:{...done,status:'in_progress',output:[]}}],['response.output_item.added',{output_index:0,item:{...done.output[0],status:'in_progress',content:[]}}],['response.content_part.added',{item_id:'msg_test',output_index:0,content_index:0,part:{type:'output_text',text:'',annotations:[]}}],['response.output_text.delta',{item_id:'msg_test',output_index:0,content_index:0,delta:'gateway-pong'}],['response.output_text.done',{item_id:'msg_test',output_index:0,content_index:0,text:'gateway-pong'}],['response.output_item.done',{output_index:0,item:done.output[0]}],['response.completed',{response:done}]];
 return new Response(events.map(([type,data],i)=>`event: ${type}\ndata: ${JSON.stringify({type,sequence_number:i,...data as object})}\n\n`).join(''),{headers:{'Content-Type':'text/event-stream'}});
}});
