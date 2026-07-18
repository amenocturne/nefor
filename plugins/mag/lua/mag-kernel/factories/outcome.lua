local kinds=require("kinds")
local M={}
local error_type={kind="named",name="nefor.contracts.AgentError",arguments={}}
local result_type={kind="union",items={{kind="variable",name="T"},error_type}}
M.declaration={name="outcome",type_variables={"T"},
  semantic={input=result_type,output=result_type,inputs={{wire="nefor.agent.Result",type=result_type}},
    outputs={{wire="nefor.outcome.Result",type=result_type}}},params={},
  inputs={outcome="nefor.agent.Result"},outputs={"nefor.outcome.Result"},signals={"kill","drain"}}
function M.construct(id,params,emit)
  local done=false; local instance={id=id}
  function instance.deliver(a)
    if done then return {status="failed",failure=kinds.Failed,value={kind="duplicate_outcome"}} end
    local message=a.messages[1].message or {}; local value=message.value
    local variant=message.variant
    if variant==nil then variant=type(value)=="table" and value.reason~=nil and "error" or "success" end
    done=true; emit({kind="nefor.outcome.Result",from=id,variant=variant,value=value}); return {status="ok"}
  end
  function instance.handle_kill() done=true end
  function instance.handle_drain() if not done then emit({kind=kinds.failed,from=id,failure=kinds.Failed,value={kind="outcome_drained"}}) end; done=true end
  emit({kind=kinds.ready,from=id}); return instance
end
return M
