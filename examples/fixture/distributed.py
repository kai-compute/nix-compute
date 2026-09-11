import json
import os
from pathlib import Path

import torch
import torch.distributed as dist
from torch.nn.parallel import DistributedDataParallel


def main():
    torch.set_num_threads(1)
    torch.manual_seed(int(os.environ["NIX_COMPUTE_SEED"]))
    parameters = json.loads(os.environ["NIX_COMPUTE_PARAMETERS"])
    dist.init_process_group("gloo")
    try:
        rank = dist.get_rank()
        world_size = dist.get_world_size()
        if rank == parameters["fail_rank"]:
            raise RuntimeError("intentional distributed fixture failure")
        samples = json.loads(
            (Path(os.environ["NIX_COMPUTE_INPUTS"]) / "dataset/samples.json").read_text()
        )[rank::world_size]
        data = torch.tensor(samples, dtype=torch.float32)
        model = DistributedDataParallel(torch.nn.Linear(1, 1, bias=False))
        optimizer = torch.optim.SGD(model.parameters(), lr=0.02)
        for _ in range(parameters["steps"]):
            optimizer.zero_grad()
            loss = torch.nn.functional.mse_loss(model(data[:, :1]), data[:, 1:])
            loss.backward()
            optimizer.step()
        weight = model.module.weight.item()
        assert abs(weight - 2.0) < 0.001, weight
        output = Path(os.environ["NIX_COMPUTE_OUTPUTS"])
        (output / "metrics.json").write_text(
            json.dumps({"rank": rank, "world_size": world_size, "weight": weight})
        )
        if rank == 0:
            (output / "model").mkdir()
            torch.save(model.module.state_dict(), output / "model/weights.pt")
            (output / "model/config.json").write_text(json.dumps({"input_features": 1}))
        print(f"rank={rank} world_size={world_size} weight={weight}", flush=True)
    finally:
        dist.destroy_process_group()


if __name__ == "__main__":
    main()
