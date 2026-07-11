local kinds=require("kinds")
local M={}
local validated={kind="named",name="core.validated.Validated",arguments={
  {kind="variable",name="E"},{kind="variable",name="T"}}}
M.declaration={name="outcome",type_variables={"T","E"},
  semantic={input=validated,output=validated,inputs={{wire="nefor.structured.Validated",type=validated}},
    outputs={{wire="nefor.outcome.Result",type=validated}}},params={},
  inputs={outcome="nefor.structured.Validated"},outputs={"nefor.outcome.Result"},signals={"kill","drain"}}
function M.construct(id,params,emit)
  local done=false; local instance={id=id}
  function instance.deliver(a)
    if done then return {status="failed",failure=kinds.Failed,value={kind="duplicate_outcome"}} end
    done=true; emit({kind="nefor.outcome.Result",from=id,value=a.messages[1].message.value}); return {status="ok"}
  end
  function instance.handle_kill() done=true end
  function instance.handle_drain() if not done then emit({kind=kinds.failed,from=id,failure=kinds.Failed,value={kind="outcome_drained"}}) end; done=true end
  emit({kind=kinds.ready,from=id}); return instance
end
return M
